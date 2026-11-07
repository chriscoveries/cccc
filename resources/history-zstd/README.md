# Sealed history sidecars

This disabled example job preserves source files and creates verified zstd
sidecars. It does not reclaim source space, change the live ledger, or enable
itself. An empty reviewed manifest contains no eligible inputs. The helper
rejects unapproved paths and refuses to infer sealing from age or a filename.

Before installation, an administrator must supply a root-owned config and
trusted parent directories, a private archive root owned by the service user,
the pinned Python environment, and a reviewed writer sealing contract and
reader inventory for every candidate. Fill each candidate's source identity,
size, mtime and SHA-256 from that contract. The service must be `Type=exec`:
`RuntimeMaxSec` does not impose a deadline on `Type=oneshot` startup. The
helper checks the active service's runtime, memory, CPU and scheduling limits;
unobservable or weaker limits hold the job. The example timer is not enabled.

Each file is bounded and compressed alone with idle I/O, nice 19, one zstd
thread and level 11. A private same-filesystem directory holds the sidecar and
receipt; the verified, fsynced directory is published with an atomic no-replace
rename. The full configured output limit is reserved before compression and
the free-space floor checked after reservation. All originals remain in place.
Unrelated concurrent writers can still consume remaining free space.

On restart, an incomplete staging directory, an invalid final bundle, or a
bundle whose source is no longer in the reviewed manifest holds the job. The
helper neither removes nor replaces any of them; use a separately approved
reconciliation procedure. Existing matching bundles are reverified and skipped.
When there are no candidates and no unresolved bundles, the job opens no source
and writes no bundle. Never point the helper at a live or mutable file.
