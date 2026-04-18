//! Rate limiter inbound par IP (FINDING-M1 — implémentation maison).
//!
//! `tower_governor` et `dashmap` sont absents du registry Kellnr.
//! Implémentation minimaliste avec `Mutex<HashMap<IpAddr, Window>>`.
//!
//! Algorithme : fenêtre glissante par minute.
//! - Chaque IP dispose d'une fenêtre de 60 secondes.
//! - Si la fenêtre est expirée, elle est réinitialisée.
//! - Le compteur est incrémenté à chaque requête.
//! - Si le compteur dépasse `max_per_minute`, retour 429.
//!
//! Le rate limit s'applique uniquement sur les endpoints POST
//! (chat/completions, embeddings) — pas sur /health, /metrics, /v1/models.
//!
//! `limit = 0` désactive entièrement le rate limiting.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Fenêtre de comptage pour une IP.
#[derive(Debug)]
struct Window {
    /// Nombre de requêtes dans la fenêtre courante.
    count: u32,
    /// Début de la fenêtre courante.
    started_at: Instant,
}

/// Rate limiter partagé par IP — thread-safe via `Arc<Mutex<...>>`.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    /// Map IP → fenêtre courante.
    windows: Arc<Mutex<HashMap<IpAddr, Window>>>,
    /// Limite de requêtes par minute. `0` = illimité.
    max_per_minute: u32,
}

/// Extrait l'IP cliente depuis les headers HTTP.
///
/// Ordre de priorité :
/// 1. `X-Forwarded-For` — premier élément (IP originale via proxy/load balancer)
/// 2. `X-Real-IP`
/// 3. Fallback : `127.0.0.1` (axum-test sans TcpListener réel, ou contexte local)
///
/// Note : en production derrière un reverse proxy, `X-Forwarded-For` est fiable
/// uniquement si le proxy est configuré pour le settre. Ne pas utiliser ConnectInfo
/// car nécessite `into_make_service_with_connect_info` incompatible avec axum-test.
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> IpAddr {
    // 1. X-Forwarded-For : "ip1, ip2, ip3" → prendre ip1 (origine)
    if let Some(forwarded_for) = headers.get("x-forwarded-for") {
        if let Ok(value) = forwarded_for.to_str() {
            if let Some(first_ip) = value.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }

    // 2. X-Real-IP
    if let Some(real_ip) = headers.get("x-real-ip") {
        if let Ok(value) = real_ip.to_str() {
            if let Ok(ip) = value.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }

    // 3. Fallback localhost
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

impl RateLimiter {
    /// Construit un rate limiter avec la limite donnée.
    ///
    /// `max_per_minute = 0` désactive le rate limiting (toutes les requêtes passent).
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            max_per_minute,
        }
    }

    /// Vérifie si une requête depuis l'IP donnée est autorisée.
    ///
    /// Retourne `true` si la requête peut passer, `false` si le quota est dépassé.
    ///
    /// # Effets de bord
    /// - Incrémente le compteur de la fenêtre courante si autorisé.
    /// - Réinitialise la fenêtre si elle est expirée (> 60 secondes).
    /// - Nettoie les entrées expirées par lots (toutes les 1000 IPs en map).
    pub fn check_and_increment(&self, ip: IpAddr) -> bool {
        // Désactivé si limit = 0.
        if self.max_per_minute == 0 {
            return true;
        }

        let mut map = self
            .windows
            .lock()
            .expect("rate limiter mutex poisoned — process should restart");

        let now = Instant::now();
        let window_duration = std::time::Duration::from_secs(60);

        // Nettoyage périodique des entrées expirées pour éviter la croissance illimitée.
        // Déclenché quand la map dépasse 1000 entrées — coût O(n) amortissable.
        if map.len() > 1000 {
            map.retain(|_, w| w.started_at.elapsed() < window_duration);
        }

        let entry = map.entry(ip).or_insert_with(|| Window {
            count: 0,
            started_at: now,
        });

        // Fenêtre expirée → réinitialiser.
        if entry.started_at.elapsed() >= window_duration {
            entry.count = 0;
            entry.started_at = now;
        }

        if entry.count >= self.max_per_minute {
            // Quota dépassé — ne pas incrémenter.
            false
        } else {
            entry.count += 1;
            true
        }
    }

    /// Retourne la limite configurée (pour exposition dans les headers).
    pub fn max_per_minute(&self) -> u32 {
        self.max_per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, a))
    }

    #[test]
    fn test_zero_limit_always_allows() {
        let rl = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(rl.check_and_increment(ip(1)));
        }
    }

    #[test]
    fn test_limit_enforced() {
        let rl = RateLimiter::new(3);
        assert!(rl.check_and_increment(ip(1))); // 1
        assert!(rl.check_and_increment(ip(1))); // 2
        assert!(rl.check_and_increment(ip(1))); // 3
        assert!(!rl.check_and_increment(ip(1))); // 4 — rejeté
    }

    #[test]
    fn test_different_ips_independent() {
        let rl = RateLimiter::new(2);
        assert!(rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(1)));
        // ip(1) est plein — ip(2) doit passer
        assert!(!rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(2)));
    }

    #[test]
    fn test_window_reset_after_expiry() {
        use std::time::Duration;

        // On ne peut pas attendre 60s en test — on vérifie la logique de reset
        // en injectant directement une fenêtre expirée.
        let rl = RateLimiter::new(2);
        {
            let mut map = rl.windows.lock().unwrap();
            map.insert(
                ip(1),
                Window {
                    count: 2,
                    // Simulate window started 61 seconds ago
                    started_at: Instant::now()
                        .checked_sub(Duration::from_secs(61))
                        .unwrap_or(Instant::now()),
                },
            );
        }
        // La fenêtre est expirée → doit être réinitialisée → la requête passe
        assert!(rl.check_and_increment(ip(1)));
    }
}
