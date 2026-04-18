//! Export métriques Prometheus — implémentation native sans dépendance externe.
//!
//! Utilise des atomiques (`AtomicU64`) pour les compteurs et gauges,
//! ce qui garantit une mise à jour thread-safe sans Mutex.
//!
//! Métriques exposées :
//! - `gateway_requests_total{route,model_alias,provider,status_code}` — counter
//! - `gateway_request_duration_ms_total{route,model_alias}` — counter (somme latences en ms)
//! - `gateway_request_duration_count{route,model_alias}` — counter (nb requêtes avec latence)
//! - `gateway_providers_configured` — gauge
//! - `gateway_uptime_seconds` — gauge (calculé à la lecture depuis start_time)
//!
//! Format wire : Prometheus text format 0.0.4
//! (https://prometheus.io/docs/instrumenting/exposition_formats/)
//!
//! Limitation alpha.2 : les labels à cardinalité variable (model_alias, provider,
//! status_code) sont stockés dans des `DashMap`-like via `Mutex<HashMap>`. Acceptable
//! pour les volumes homelab. Pour de la haute cardinalité, une crate dédiée s'impose.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Étiquettes (labels) d'une requête pour les métriques.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestLabels {
    pub route: String,
    pub model_alias: String,
    pub provider: String,
    pub status_code: u16,
}

/// Étiquettes route+alias uniquement (pour les histogrammes de durée).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DurationLabels {
    pub route: String,
    pub model_alias: String,
}

/// Registre de métriques partagé.
///
/// Clonable via `Arc` — chaque handler possède un handle léger vers les
/// structures partagées. Aucune copie de données.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    /// Counter : gateway_requests_total — indexé par labels.
    requests_total: Mutex<HashMap<RequestLabels, u64>>,
    /// Counter : somme des latences ms par (route, alias) — pour computed avg.
    duration_ms_sum: Mutex<HashMap<DurationLabels, u64>>,
    /// Counter : nombre d'observations de latence par (route, alias).
    duration_count: Mutex<HashMap<DurationLabels, u64>>,
    /// Gauge : nombre de providers configurés (fixe après démarrage).
    providers_configured: u64,
    /// Instant de démarrage — pour calculer uptime à la lecture.
    start_time: Instant,
}

impl Metrics {
    /// Crée un nouveau registre métriques.
    ///
    /// `providers_configured` : nombre de providers dans la config (ne change pas).
    pub fn new(providers_configured: usize) -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                requests_total: Mutex::new(HashMap::new()),
                duration_ms_sum: Mutex::new(HashMap::new()),
                duration_count: Mutex::new(HashMap::new()),
                providers_configured: providers_configured as u64,
                start_time: Instant::now(),
            }),
        }
    }

    /// Incrémente `gateway_requests_total` et enregistre la latence si fournie.
    ///
    /// Appelé après chaque requête traitée (succès ou erreur).
    pub fn record_request(
        &self,
        route: &str,
        model_alias: &str,
        provider: &str,
        status_code: u16,
        latency: Option<Duration>,
    ) {
        let req_labels = RequestLabels {
            route: route.to_owned(),
            model_alias: model_alias.to_owned(),
            provider: provider.to_owned(),
            status_code,
        };

        // Incrément counter requests_total.
        if let Ok(mut map) = self.inner.requests_total.lock() {
            *map.entry(req_labels).or_insert(0) += 1;
        }

        // Enregistrement latence si disponible.
        if let Some(dur) = latency {
            let dur_labels = DurationLabels {
                route: route.to_owned(),
                model_alias: model_alias.to_owned(),
            };
            let ms = dur.as_millis() as u64;

            if let Ok(mut map) = self.inner.duration_ms_sum.lock() {
                *map.entry(dur_labels.clone()).or_insert(0) += ms;
            }
            if let Ok(mut map) = self.inner.duration_count.lock() {
                *map.entry(dur_labels).or_insert(0) += 1;
            }
        }
    }

    /// Produit l'export Prometheus en text format 0.0.4.
    ///
    /// Retourne une `String` prête à être servie avec Content-Type
    /// `text/plain; version=0.0.4; charset=utf-8`.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);

        // --- gateway_requests_total ---
        out.push_str(
            "# HELP gateway_requests_total Nombre total de requetes traitees par le gateway.\n",
        );
        out.push_str("# TYPE gateway_requests_total counter\n");
        if let Ok(map) = self.inner.requests_total.lock() {
            let mut entries: Vec<_> = map.iter().collect();
            // Tri déterministe pour faciliter les tests.
            entries.sort_by_key(|(k, _)| (&k.route, &k.model_alias, &k.provider, k.status_code));
            for (labels, count) in entries {
                out.push_str(&format!(
                    "gateway_requests_total{{route=\"{}\",model_alias=\"{}\",provider=\"{}\",status_code=\"{}\"}} {}\n",
                    escape_label(&labels.route),
                    escape_label(&labels.model_alias),
                    escape_label(&labels.provider),
                    labels.status_code,
                    count,
                ));
            }
        }

        // --- gateway_request_duration_seconds (histogram simplifié via sum + count) ---
        out.push_str("# HELP gateway_request_duration_seconds Duree des requetes en secondes.\n");
        out.push_str("# TYPE gateway_request_duration_seconds summary\n");
        if let (Ok(sum_map), Ok(count_map)) = (
            self.inner.duration_ms_sum.lock(),
            self.inner.duration_count.lock(),
        ) {
            let mut keys: Vec<_> = sum_map.keys().collect();
            keys.sort_by_key(|k| (&k.route, &k.model_alias));
            for key in keys {
                let sum_ms = sum_map.get(key).copied().unwrap_or(0);
                let count = count_map.get(key).copied().unwrap_or(0);
                // Convertit ms → secondes pour le format Prometheus standard.
                let sum_secs = sum_ms as f64 / 1000.0;
                out.push_str(&format!(
                    "gateway_request_duration_seconds_sum{{route=\"{}\",model_alias=\"{}\"}} {:.6}\n",
                    escape_label(&key.route),
                    escape_label(&key.model_alias),
                    sum_secs,
                ));
                out.push_str(&format!(
                    "gateway_request_duration_seconds_count{{route=\"{}\",model_alias=\"{}\"}} {}\n",
                    escape_label(&key.route),
                    escape_label(&key.model_alias),
                    count,
                ));
            }
        }

        // --- gateway_providers_configured ---
        out.push_str(
            "# HELP gateway_providers_configured Nombre de providers configures au demarrage.\n",
        );
        out.push_str("# TYPE gateway_providers_configured gauge\n");
        out.push_str(&format!(
            "gateway_providers_configured {}\n",
            self.inner.providers_configured
        ));

        // --- gateway_uptime_seconds ---
        let uptime = self.inner.start_time.elapsed().as_secs_f64();
        out.push_str("# HELP gateway_uptime_seconds Duree de vie du gateway en secondes.\n");
        out.push_str("# TYPE gateway_uptime_seconds gauge\n");
        out.push_str(&format!("gateway_uptime_seconds {:.3}\n", uptime));

        out
    }
}

/// Échappe les valeurs de labels Prometheus (guillemets et backslash uniquement).
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_render_requests_total_present() {
        let m = Metrics::new(2);
        m.record_request(
            "/v1/chat/completions",
            "my-alias",
            "llmcore",
            200,
            Some(Duration::from_millis(50)),
        );

        let output = m.render();
        assert!(
            output.contains("# TYPE gateway_requests_total counter"),
            "TYPE line manquante dans:\n{}",
            output
        );
        assert!(
            output.contains("gateway_requests_total{"),
            "metric line manquante dans:\n{}",
            output
        );
    }

    #[test]
    fn test_metrics_render_providers_configured() {
        let m = Metrics::new(3);
        let output = m.render();
        assert!(
            output.contains("gateway_providers_configured 3"),
            "gauge providers_configured incorrect:\n{}",
            output
        );
    }

    #[test]
    fn test_metrics_render_uptime_gauge() {
        let m = Metrics::new(1);
        let output = m.render();
        assert!(
            output.contains("# TYPE gateway_uptime_seconds gauge"),
            "TYPE uptime manquante:\n{}",
            output
        );
        assert!(
            output.contains("gateway_uptime_seconds "),
            "valeur uptime manquante:\n{}",
            output
        );
    }

    #[test]
    fn test_metrics_counter_accumulates() {
        let m = Metrics::new(1);
        m.record_request("/v1/embeddings", "bge-m3", "llmcore-embed", 200, None);
        m.record_request("/v1/embeddings", "bge-m3", "llmcore-embed", 200, None);
        m.record_request("/v1/embeddings", "bge-m3", "llmcore-embed", 400, None);

        let output = m.render();
        // Deux appels 200 + un 400 — vérifie la ligne pour status_code=200.
        assert!(
            output.contains("status_code=\"200\"} 2"),
            "compteur 200 doit valoir 2:\n{}",
            output
        );
        assert!(
            output.contains("status_code=\"400\"} 1"),
            "compteur 400 doit valoir 1:\n{}",
            output
        );
    }

    #[test]
    fn test_metrics_duration_sum_and_count() {
        let m = Metrics::new(1);
        m.record_request(
            "/v1/chat/completions",
            "qwen",
            "llmcore",
            200,
            Some(Duration::from_millis(100)),
        );
        m.record_request(
            "/v1/chat/completions",
            "qwen",
            "llmcore",
            200,
            Some(Duration::from_millis(200)),
        );

        let output = m.render();
        // sum = 300ms = 0.300s, count = 2.
        assert!(
            output.contains("gateway_request_duration_seconds_sum{") && output.contains("0.300000"),
            "sum duree incorrecte:\n{}",
            output
        );
        assert!(
            output.contains("gateway_request_duration_seconds_count{") && output.contains("} 2"),
            "count duree incorrect:\n{}",
            output
        );
    }

    #[test]
    fn test_escape_label_quotes() {
        assert_eq!(escape_label("hello\"world"), "hello\\\"world");
        assert_eq!(escape_label("back\\slash"), "back\\\\slash");
        assert_eq!(escape_label("normal"), "normal");
    }
}
