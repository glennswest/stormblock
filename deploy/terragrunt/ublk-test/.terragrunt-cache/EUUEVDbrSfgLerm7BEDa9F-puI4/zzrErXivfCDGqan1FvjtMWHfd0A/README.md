# terraform-modules

Shared, versioned Terraform modules for the home infrastructure — so projects
**reference** common infrastructure instead of copy-pasting it. Paired with a
Terragrunt setup that defines provider/version/state boilerplate once.

**Version:** 0.3.0

## Quickstart for teammates

Provision Proxmox VMs with the shared module — don't copy `.tf` files. Full
guide: [`docs/USAGE.md`](docs/USAGE.md).

**One-time setup:**
1. `brew install hashicorp/tap/terraform terragrunt`
2. Make sure the Proxmox node's `root` trusts your `~/.ssh/id_rsa.pub`
3. Get an API token on the node: `pveum user token add root@pam <yourname> --privsep 0`, then:
   ```bash
   export PROXMOX_API_TOKEN='root@pam!yourname=<the-value>'
   ```

**To create VMs:** copy `examples/terragrunt/` and edit a unit. Each VM needs a
unique `vm_id` (check `qm list`), a unique MAC `BC:24:11:xx:xx:xx`, and an IP
outside the DHCP pool (g8 pool is `.100–.200`, so use `.10–.99`):

```hcl
include "root" { path = find_in_parent_folders("root.hcl") }

terraform {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]
  vms = {
    web1 = { vm_id = 150, mac = "BC:24:11:08:00:10", ip = "192.168.8.50" }
  }
}
```

```bash
cd your-unit && terragrunt init && terragrunt apply
```

You get a Fedora VM on its reserved IP, DNS auto-registered, reachable at
`ssh fedora@web1.g8.lo`. `terragrunt destroy` cleans up the VM, the DHCP
reservation, and the DNS records. **Always pin `?ref=` to a tag** (currently
`v0.1.0`) — never track `main`.

## Why

Every project that spins up Proxmox VMs was copying the same provider config, VM
resource, cloud-init wiring, and MicroDNS DHCP/DNS logic. That drift is what
modules exist to prevent: fix a bug once here, tag a release, and consumers opt
in by bumping `?ref=`.

## Modules

| Module | Purpose |
|--------|---------|
| [`proxmox-fedora-vm`](modules/proxmox-fedora-vm/) | Fleet of Fedora Cloud VMs on Proxmox with fixed-MAC MicroDNS DHCP reservations, DNS auto-register, and DNS cleanup on destroy |

## Consume from another project

```hcl
module "vms" {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"

  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]
  vms = {
    web1 = { vm_id = 150, mac = "BC:24:11:08:00:10", ip = "192.168.8.50" }
  }
}
```

Pin `?ref=` to a tag (`v0.1.0`) so upgrades are deliberate.

## Terragrunt (kills the remaining boilerplate)

[`examples/terragrunt/`](examples/terragrunt/) shows the recommended layout: a
single `root.hcl` generates the Proxmox provider and remote-state config for
every unit; each unit is ~15 lines (include root, source a module, set inputs).

```bash
export PROXMOX_API_TOKEN='root@pam!terraform=...'
cd examples/terragrunt/g8/bits && terragrunt apply
```

## Releasing

Semantic versioning. Tag the repo (`vMAJOR.MINOR.PATCH`); consumers reference
tags via `?ref=`. See `CHANGELOG.md`.
