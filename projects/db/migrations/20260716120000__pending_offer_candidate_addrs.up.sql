-- candidate_addrs: CSV of the inviter's self-advertised reachable addresses,
-- carried on the offer so the joiner can try each (pinned to the bootstrap fp)
-- for join-confirm instead of relying solely on the TLS source IP — which is
-- unreliable when the inviter reaches the joiner over a tunnel/VPN (the source
-- IP is then the tunnel address, not the inviter's bootstrap listener).
ALTER TABLE pod_pending_offers ADD COLUMN candidate_addrs TEXT NOT NULL DEFAULT '';
