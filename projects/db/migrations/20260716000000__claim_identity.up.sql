-- claim_identity: stable orca UUIDv7 for each non-peer child a host claims to
-- run (docker container, proxmox vm/lxc, …). The provider-native id and the
-- other claim fields are natural-key ATTRIBUTES used to find/correlate a claim
-- across reporting peers — the orca id itself is a minted UUIDv7, never derived
-- from those fields. Minted once by the source peer (the one holding the
-- provider creds) and reported on TopologyClaim.uuid so every viewer agrees.
CREATE TABLE IF NOT EXISTS claim_identity (
    provider          TEXT NOT NULL,
    provider_instance TEXT NOT NULL,
    kind              TEXT NOT NULL,
    native_id         TEXT NOT NULL,
    uuid              TEXT NOT NULL,
    minted_at         INTEGER NOT NULL,
    PRIMARY KEY (provider, provider_instance, kind, native_id)
);
CREATE INDEX IF NOT EXISTS idx_claim_identity_uuid ON claim_identity(uuid);
