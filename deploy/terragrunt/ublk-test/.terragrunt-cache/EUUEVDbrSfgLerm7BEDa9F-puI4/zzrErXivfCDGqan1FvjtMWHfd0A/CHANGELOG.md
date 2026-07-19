# Changelog

## [Unreleased]

## [v0.3.0] — 2026-07-08

### Changed
- **BREAKING:** default `vm_id_min`/`vm_id_max` moved from `131`-`199` to
  `2000`-`2100`. The pool/ACL scoping from v0.2.0 already makes this module
  structurally unable to touch VMs outside its pool, but the low 100s/900s
  overlap with decades of hand-created infra by number alone; a 2000+ block
  has zero chance of ever being picked by hand. Consumers must update their
  `vms` maps to the new range and re-apply (this replaces every managed VM).

## [v0.2.0] — 2026-07-08

### Added
- **feat:** `pool_id` variable (default `terraform-managed`) — every VM the
  module creates is placed in a dedicated Proxmox resource pool.
- **feat:** `vm_id_min`/`vm_id_max` variables (default `131`/`199`) with a
  validation rule on `var.vms` rejecting any `vm_id` outside the range.

### Breaking
- **BREAKING:** Consumers must now use a Proxmox API token that is ACL-scoped
  to exactly the `pool_id` pool (not root@pam, not any broader-scoped token).
  Root cause and full context: see "Incident: 2026-07-08" in `CLAUDE.md`. A
  hardcoded, uncoordinated `vm_id` collided with a hand-created VM outside any
  IaC tracking, and an unrestricted root@pam token destroyed it with no
  backup. This release closes the hole at the permission layer, not just the
  config layer.

### 2026-06-29
- **docs:** USAGE — document the 4-VM create/update/verify/destroy reference workflow; remove unrelated cross-project reference.
- **docs:** README — add "Quickstart for teammates" section.
- **docs:** Add `docs/USAGE.md` — onboarding/usage directions for other users.

## [v0.1.0] — 2026-06-29

### Added
- `proxmox-fedora-vm` module — provisions a fleet of Fedora Cloud VMs on Proxmox
  (`bpg/proxmox`): disk imported from the Fedora qcow2, fixed MAC per VM, MicroDNS
  DHCP reservation pinning MAC→IP with DNS auto-registration, cloud-init (built-in
  template or caller-supplied `user_data`), DHCP networking. Destroy removes the
  reservation and the auto-created DNS records (A delete cascades the PTR).
- Terragrunt example (`examples/terragrunt/`) — `root.hcl` generates the Proxmox
  provider and remote-state config once; `g8/bits/` consumes the module in ~15
  lines. Token supplied via `PROXMOX_API_TOKEN` env, never committed.
- Repo docs: README, module README, CLAUDE.md.

### Notes
- Extracted from the proven `terraform8` configuration so projects reference a
  versioned module instead of copy-pasting.
