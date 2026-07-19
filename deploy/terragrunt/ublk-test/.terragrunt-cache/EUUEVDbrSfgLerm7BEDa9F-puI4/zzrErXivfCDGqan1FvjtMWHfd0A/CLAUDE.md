# CLAUDE.md — terraform-modules

Shared, versioned Terraform modules + Terragrunt scaffolding for the home infra.
Inherits the cross-project rules in `../CLAUDE.md`.

## What this is

The single source of truth for reusable Terraform. Projects reference modules
here by Git tag (`?ref=vX.Y.Z`) instead of copying `.tf` files. Originated by
extracting the proven `terraform8` Fedora/Proxmox/MicroDNS pattern.

## Version

**Current: 0.3.0**

### Version locations (keep in sync)
- `README.md` — "Version:" line
- `CLAUDE.md` — this section
- `CHANGELOG.md` — latest release heading
- Git tag — `vX.Y.Z` (this is what consumers pin via `?ref=`)

## Layout

```
modules/proxmox-fedora-vm/   Fedora VM fleet + MicroDNS reservation + DNS cleanup
examples/terragrunt/         root.hcl (provider/state once) + g8/bits unit
```

## Conventions

- **Modules never configure a provider** — only `required_providers` in
  `versions.tf`. The root module (or Terragrunt `generate`) configures it.
- **Every change is a release**: bump version in all locations, tag `vX.Y.Z`,
  push the tag. Consumers upgrade by bumping `?ref=`.
- Keep modules composable and parameterized; g8 defaults are convenience only.

## Work plan / status

- [x] Extract `proxmox-fedora-vm` module from terraform8 (validated)
- [x] Terragrunt example (`root.hcl` + `g8/bits`) — init + validate pass
- [x] Tag v0.1.0
- [x] Blast-radius guardrails (`pool_id` + `vm_id_min`/`vm_id_max` validation) — v0.2.0
- [ ] (optional) Migrate `terraform8` to consume this module as the reference
- [ ] (optional) Remote state backend (MinIO/S3) wired in `root.hcl`
- [ ] (optional) `proxmox-debian-vm` / service-specific modules as needed

## Incident: 2026-07-08 — VMID collision destroyed a hand-created VM

`irondirectory`'s `phase1-verify` unit hardcoded `vm_id = 140` for its
`memberhost` VM. VMID 140 was already in use by a hand-created GPU VM
(`ai.g8.lo`, not managed by any IaC). A `terraform apply` under a root@pam API
token — which has **no ACL restriction at all**, since root@pam bypasses
Proxmox's permission system unconditionally — destroyed and recreated VM 140
to match `phase1-verify`'s desired state, permanently deleting the GPU VM's
disk with no backup. Full root-cause writeup lives in the consuming project's
history, not here.

**Fix, two independent layers (both required — neither alone is sufficient):**
1. Every VM this module creates now gets `pool_id` set to a dedicated Proxmox
   resource pool. Consumers must use a Proxmox API token that is ACL-scoped to
   *only* that pool (not root@pam, not any token with a broader grant) — this
   makes it structurally impossible, at the Proxmox permission layer, for this
   module to touch a VM outside the pool, no matter what the Terraform config
   says.
2. `vm_id_min`/`vm_id_max` validation on `var.vms` — defense in depth so a
   bad/reused id fails `terraform plan` before it reaches the API at all.

Never run this module against a root@pam token again.

## Notes

- Module + Terragrunt validated with `terragrunt init/validate` (bpg downloaded,
  config valid). Not applied from here — the pattern is proven live in terraform8.
- Token is never committed: `export PROXMOX_API_TOKEN='root@pam!terraform=...'`.
