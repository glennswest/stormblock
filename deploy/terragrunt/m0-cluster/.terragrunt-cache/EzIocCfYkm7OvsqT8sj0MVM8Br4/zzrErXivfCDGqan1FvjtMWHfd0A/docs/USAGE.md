# Usage — directions for other users

How to provision Proxmox VMs with these shared modules. Two ways to consume:
**Terragrunt** (recommended — no boilerplate) or a **plain Terraform** `module {}`
block. Pick one.

---

## 1. One-time prerequisites

| Need | How |
|------|-----|
| Terraform `>= 1.6` | `brew install hashicorp/tap/terraform` |
| Terragrunt (if using it) | `brew install terragrunt` |
| `curl` + `python3` | Used by the module to call the MicroDNS REST API (preinstalled on macOS) |
| SSH key authorized on the Proxmox node | The node's `root` must trust your `~/.ssh/id_rsa.pub` (the provider SSHes in to upload cloud-init + import the disk) |
| A Proxmox API token | See below |
| The Fedora cloud image staged on the node | See below |

### Get a Proxmox API token

On the Proxmox node (`pve.g8.lo`):

```bash
pveum user token add root@pam <yourname> --privsep 0
```

Copy the returned `value`. Your full token string is:
`root@pam!<yourname>=<value>`. Export it (never commit it):

```bash
export PROXMOX_API_TOKEN='root@pam!yourname=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'
```

### Stage the Fedora image (only if it's not already there)

The module imports the disk from an `import`-content datastore. On the node:

```bash
pvesm set local --content iso,vztmpl,snippets,backup,import   # once, enables import
mkdir -p /var/lib/vz/import
# drop a Fedora Cloud Base qcow2 in /var/lib/vz/import/
pvesm list local --content import      # confirm it shows up
```

Default the module expects: `local:import/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2`
(override with the `fedora_image` input).

---

## 2. Choose your IDs — the conventions that keep things collision-free

Each VM needs three unique values you pick up front:

| Field | Rule |
|-------|------|
| `vm_id` | Unused Proxmox VMID. Check with `qm list` on the node. |
| `mac` | Fixed MAC, Proxmox OUI `BC:24:11:xx:xx:xx`. This is the key the DHCP reservation is built on — **must be unique**. |
| `ip` | An address in the subnet but **outside** the DHCP dynamic pool (g8 pool is `192.168.8.100-200`, so reservations use `.10-.99`). Check it's free: `ping` it / look at existing reservations (`curl $MICRODNS/dhcp/reservations`). |

The hostname is the map key; the FQDN becomes `<key>.<search_domain>` (e.g.
`web1.g8.lo`) and DNS is auto-registered from the reservation.

---

## 3a. Consume via Terragrunt (recommended)

Copy `examples/terragrunt/` as your starting point. A unit is one fleet:

```
your-infra/
  root.hcl                 # provider + state, defined ONCE (copy from examples)
  g8/
    web/terragrunt.hcl     # one fleet
```

`g8/web/terragrunt.hcl`:

```hcl
include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]

  vms = {
    web1 = { vm_id = 150, mac = "BC:24:11:08:00:10", ip = "192.168.8.50" }
    web2 = { vm_id = 151, mac = "BC:24:11:08:00:11", ip = "192.168.8.51" }
  }
}
```

Run it:

```bash
export PROXMOX_API_TOKEN='root@pam!yourname=...'
cd g8/web
terragrunt init
terragrunt plan
terragrunt apply
# ... when done:
terragrunt destroy
```

`terragrunt run-all <cmd>` from the repo root runs every unit at once.

## 3b. Consume via plain Terraform (no Terragrunt)

```hcl
# versions.tf is provided by the module; you provide the provider config:
provider "proxmox" {
  endpoint  = "https://pve.g8.lo:8006/"
  api_token = var.proxmox_api_token         # set TF_VAR_proxmox_api_token
  insecure  = true
  ssh {
    agent       = false
    username    = "root"
    private_key = file(pathexpand("~/.ssh/id_rsa"))
  }
}
variable "proxmox_api_token" { type = string, sensitive = true }

module "web" {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"

  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc"
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]
  vms = {
    web1 = { vm_id = 150, mac = "BC:24:11:08:00:10", ip = "192.168.8.50" }
  }
}
```

```bash
export TF_VAR_proxmox_api_token='root@pam!yourname=...'
terraform init && terraform plan && terraform apply
```

---

## 4. What you get / what happens on destroy

- Each VM boots Fedora via DHCP and receives its **reserved** IP.
- DNS forward (`A`) + reverse (`PTR`) are auto-registered.
- `qemu-guest-agent` is installed; SSH in as `fedora@<host>.g8.lo` with your key.
- `destroy` removes the VM, the DHCP reservation, **and** the DNS records.

Custom per-VM cloud-init: set `user_data` on a VM entry to a fully-rendered
cloud-config string (e.g. via `templatefile(...)`) to override the built-in
template — useful when a VM needs extra packages, files, or service setup.

---

## 4b. Reference workflow — the 4-VM lifecycle

This is the test the module was built and validated against: **create 4 VMs from
the cloud image, update them, verify them, then destroy them.** The `g8/bits`
unit (`bit1`–`bit4`) is exactly this fleet.

```bash
export PROXMOX_API_TOKEN='root@pam!yourname=...'
cd examples/terragrunt/g8/bits

# 1. Create — 4 Fedora VMs on reserved IPs .91-.94, DNS auto-registered
terragrunt apply

# 2. Update — dnf update on each (parallel)
for h in bit1 bit2 bit3 bit4; do
  ssh fedora@$h.g8.lo 'sudo dnf -y update' &
done; wait

# 3. Verify — OS, guest agent, cloud-init, networking
for h in bit1 bit2 bit3 bit4; do
  echo "== $h =="
  ssh fedora@$h.g8.lo '. /etc/os-release; echo "$PRETTY_NAME $(uname -r)"; \
    echo "agent: $(systemctl is-active qemu-guest-agent)  cloud-init: $(cloud-init status)"'
done

# 4. Destroy — removes VMs, DHCP reservations, AND DNS records
terragrunt destroy
```

Expected: each VM comes up as Fedora on its reserved IP, resolves at
`<host>.g8.lo`, updates cleanly, and leaves nothing behind after destroy
(no orphaned VM, reservation, or DNS record).

---

## 5. Pinning & upgrades

Always pin the module to a tag: `?ref=v0.1.0`. To pick up fixes (e.g. when
[microdns#2](https://github.com/glennswest/microdns/issues/2) lands and the
DNS-cleanup workaround is removed), bump `?ref=` to the new tag and re-`init`.
Never track `main` in real environments.

## 6. Common gotchas

- **"unable to parse directory volume name … .qcow2"** → the image isn't on an
  `import`-content datastore. See §1.
- **SSH handshake / "no supported methods"** → the node's `root` doesn't trust
  your key, or `private_key` points at the wrong file. The provider ignores
  `~/.ssh/config`.
- **VM gets a `.100-.200` pool IP instead of its reserved one** → the reservation
  didn't exist before boot, or the MAC doesn't match. The module orders this
  correctly; if hand-rolling, create the reservation first.
- **Name still resolves after destroy** → only relevant if you bypassed the
  module's destroy step; see microdns#2.
