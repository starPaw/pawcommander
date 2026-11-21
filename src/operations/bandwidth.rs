use crate::vfs::Endpoint;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TransferLimits {
    pub global_kib: u32,
    connections: Vec<ConnectionLimit>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ConnectionLimit {
    endpoint: Endpoint,
    kib: u32,
}

impl TransferLimits {
    pub fn endpoints(&self) -> impl Iterator<Item = &Endpoint> {
        self.connections.iter().map(|limit| &limit.endpoint)
    }

    pub fn connection_kib(&self, endpoint: &Endpoint) -> u32 {
        self.connections
            .iter()
            .find(|limit| limit.endpoint.same_authority(endpoint))
            .map_or(0, |limit| limit.kib)
    }

    pub fn set_connection_kib(&mut self, endpoint: &Endpoint, kib: u32) {
        self.connections
            .retain(|limit| !limit.endpoint.same_authority(endpoint));
        if kib != 0 {
            self.connections.push(ConnectionLimit {
                endpoint: Endpoint {
                    key: None,
                    jump: None,
                    ..endpoint.clone()
                },
                kib,
            });
        }
    }

    pub(super) fn copy_rate(&self, endpoints: [Option<&Endpoint>; 2]) -> u64 {
        let [source, target] = endpoints;
        let shared = matches!((source, target), (Some(a), Some(b)) if a.same_authority(b));
        let connection_rates = endpoints.into_iter().flatten().map(|endpoint| {
            let rate = u64::from(self.connection_kib(endpoint)) * 1024;
            // A remote-to-remote copy can use one connection in both directions.
            if shared {
                rate / 2
            } else {
                rate
            }
        });
        self.effective_rate(connection_rates)
    }

    pub(super) fn readback_rate(&self, target: Option<&Endpoint>) -> u64 {
        let Some(target) = target else {
            return 0;
        };
        self.effective_rate([u64::from(self.connection_kib(target)) * 1024])
    }

    fn effective_rate(&self, connections: impl IntoIterator<Item = u64>) -> u64 {
        std::iter::once(u64::from(self.global_kib) * 1024)
            .chain(connections)
            .filter(|&rate| rate != 0)
            .min()
            .unwrap_or(0)
    }
}

/// Accumulate at most 50 ms of credit to keep bursts and wait times bounded.
#[derive(Default)]
pub(super) struct Pacer {
    rate: u64,
    updated: Option<Instant>,
    credit: f64,
}

impl Pacer {
    pub fn block(&mut self, maximum: u64, rate: u64, now: Instant) -> Result<u64, Duration> {
        if rate != self.rate {
            *self = Self {
                rate,
                ..Self::default()
            };
        }
        if rate == 0 {
            return Ok(maximum);
        }
        let capacity = (rate / 20).max(1);
        let wanted = maximum.min(capacity);
        let previous = self.updated.replace(now).unwrap_or(now);
        let earned = now.saturating_duration_since(previous).as_secs_f64() * rate as f64;
        self.credit = (self.credit + earned).min(capacity as f64);
        if self.credit >= wanted as f64 {
            Ok(wanted)
        } else {
            Err(
                Duration::from_secs_f64((wanted as f64 - self.credit) / rate as f64)
                    .max(Duration::from_nanos(1)),
            )
        }
    }

    pub fn spend(&mut self, bytes: u64) {
        self.credit = (self.credit - bytes as f64).max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(user: &str) -> Endpoint {
        Endpoint {
            host: "server.example".into(),
            port: 2202,
            user: user.into(),
            key: Some("/test/ssh-key".into()),
            jump: None,
        }
    }

    #[test]
    fn transfer_speed_uses_global_and_route_specific_limits() {
        let first = endpoint("upload");
        let second = endpoint("download");
        let alternate_key = Endpoint {
            key: Some("/test/other-key".into()),
            ..first.clone()
        };
        let mut limits = TransferLimits::default();
        assert_eq!(limits.copy_rate([None, None]), 0);
        limits.global_kib = 100;
        limits.set_connection_kib(&first, 60);
        limits.set_connection_kib(&second, 120);
        assert_eq!(limits.copy_rate([None, None]), 100 * 1024);
        assert_eq!(limits.copy_rate([Some(&alternate_key), None]), 60 * 1024);
        assert_eq!(limits.copy_rate([None, Some(&second)]), 100 * 1024);
        assert_eq!(limits.copy_rate([Some(&first), Some(&second)]), 60 * 1024);
        assert_eq!(
            limits.copy_rate([Some(&first), Some(&alternate_key)]),
            30 * 1024
        );
        assert_eq!(limits.readback_rate(Some(&second)), 100 * 1024);
        assert_eq!(limits.readback_rate(None), 0);
        assert!(limits.endpoints().all(|endpoint| endpoint.key.is_none()));
        limits.set_connection_kib(&alternate_key, 0);
        assert_eq!(limits.copy_rate([Some(&first), Some(&second)]), 100 * 1024);
    }

    #[test]
    fn transfer_speed_credit_is_bounded_and_existing_credit_is_replaced_on_edit() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let interval = Duration::from_millis(50);
        let fast = 20_000;
        assert_eq!(pacer.block(512, 0, start), Ok(512));
        assert_eq!(pacer.block(8192, fast, start), Err(interval));
        assert_eq!(pacer.block(8192, fast, start + interval), Ok(1_000));
        pacer.spend(400);
        assert_eq!(pacer.block(600, fast, start + interval), Ok(600));
        pacer.spend(600);
        assert_eq!(pacer.block(8192, fast, start + interval), Err(interval));
        let resumed = start + Duration::from_secs(10);
        assert_eq!(pacer.block(8192, fast, resumed), Ok(1_000));
        assert_eq!(pacer.block(8192, 1_000, resumed), Err(interval));
        assert_eq!(pacer.block(8192, 1_000, resumed + interval), Ok(50));
        assert_eq!(pacer.block(8192, 0, resumed + interval), Ok(8192));
    }
}
