# Shared integration provenance

Restored frame.rs and frame/object_table.rs from hash-verified post-checkpoint candidates, selected records in integration-frame-sources.json. These are fingerprint/session-replayed later sources, not exact checkpoint originals. Prior candidate baseline archived at /home/zn/asterinas-build-env/wal-ext4-integration/pre-integration-rnvpc.tgz.

The metadata_mutation_guard wakeup test previously failed compilation because this API was absent (prior integration/ktest.log). It is restored into tests.rs alongside the matching later implementation. Parent full build will validate coupling to restored leaf-level completion.

Current follow-up: connect independent rNVPC WAL recovery and ensure rNVPCe root validation before reclamation.
