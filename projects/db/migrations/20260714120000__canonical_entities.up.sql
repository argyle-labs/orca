-- Canonical-entity registry. One row per real thing (host, container, VM, LXC,
-- cluster, service, …), identified by a uuidv7 we mint and own. Provider/native
-- ids (docker id, proxmox vmid, host machine_id, mac, …) are NOT the identity —
-- they are references that resolve TO a canonical entity. This is what makes the
-- same real thing seen via multiple providers (docker + dockge + unraid) collapse
-- onto one canonical id, and survive a native-id change (recreated container).
CREATE TABLE IF NOT EXISTS entities (
    id          TEXT PRIMARY KEY,   -- uuidv7, minted by us; the canonical identity
    kind        TEXT NOT NULL,      -- host | container | vm | lxc | cluster | service | stack
    label       TEXT,               -- display name (mutable; never an identity)
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- A reference is a (kind, value) pair a provider knows the entity by. Globally
-- unique on (ref_kind, ref_value): a given reference resolves to exactly ONE
-- canonical entity. Deleting an entity cascades its references away.
CREATE TABLE IF NOT EXISTS entity_refs (
    ref_kind    TEXT NOT NULL,      -- docker_id | proxmox_vmid | machine_id | mac | dockge_stack | …
    ref_value   TEXT NOT NULL,
    entity_id   TEXT NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    source      TEXT,               -- provider/discovery path that contributed it (a controller path)
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (ref_kind, ref_value)
);

CREATE INDEX IF NOT EXISTS idx_entity_refs_entity ON entity_refs(entity_id);
