# Module: `proxmox-fedora-vm`

Provision a fleet of Fedora Cloud VMs on a Proxmox node (`bpg/proxmox`), each
pinned to a fixed MAC with a **MicroDNS DHCP reservation** that supplies the IP
and auto-registers DNS. Boots via DHCP; `destroy` removes the reservation **and**
the auto-created DNS records.

## Usage

```hcl
provider "proxmox" {
  endpoint  = "https://pve.g8.lo:8006/"
  api_token = var.proxmox_api_token
  insecure  = true
  ssh {
    agent       = false
    username    = "root"
    private_key = file(pathexpand("~/.ssh/id_rsa"))
  }
}

module "bits" {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"

  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]

  vms = {
    bit1 = { vm_id = 141, mac = "BC:24:11:08:00:01", ip = "192.168.8.91" }
    bit2 = { vm_id = 142, mac = "BC:24:11:08:00:02", ip = "192.168.8.92" }
  }
}
```

## Inputs (key)

| Name | Required | Default | Purpose |
|------|----------|---------|---------|
| `vms` | yes | — | Map of VMs: `vm_id`, `mac`, `ip` (+ optional `cores`/`memory`/`disk_size`/`user_data`) |
| `dns_zone_id` | yes | — | MicroDNS zone ID for `search_domain` (DNS cleanup on destroy) |
| `ci_ssh_public_keys` | no | `[]` | Keys injected into the default user (built-in template) |
| `fedora_image` | no | `local:import/Fedora-...43-1.6...qcow2` | qcow2 to import (needs `import` content type) |
| `node_name` / `vm_datastore` / `snippet_datastore` / `network_bridge` | no | g8 defaults | Proxmox placement |
| `search_domain` / `microdns_base_url` | no | g8 defaults | DNS/network |
| `ci_user` / `tags` | no | `fedora` / `[terraform,fedora]` | cloud-init / tagging |

Per-VM `user_data` lets a caller supply a fully-rendered cloud-config (e.g. an
app or service VM) instead of the built-in template.

## Requirements

- Terraform `>= 1.6`, provider `bpg/proxmox >= 0.66`
- The Fedora qcow2 present on the node under an `import`-content datastore
- `curl` + `python3` on the machine running Terraform (reservation/DNS calls)
- SSH access to the node (snippet upload / disk import)
