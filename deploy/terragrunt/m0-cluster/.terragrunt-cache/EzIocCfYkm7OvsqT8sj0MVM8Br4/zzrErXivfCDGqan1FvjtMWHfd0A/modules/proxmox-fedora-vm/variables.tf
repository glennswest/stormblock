# ─── Proxmox placement ───────────────────────────────────────────────────────

variable "node_name" {
  description = "Proxmox node to create resources on"
  type        = string
  default     = "pve"
}

variable "fedora_image" {
  description = "Datastore volume ID of the Fedora cloud qcow2 to import from (import content type)"
  type        = string
  default     = "local:import/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2"
}

variable "vm_datastore" {
  description = "Datastore for VM disks and the cloud-init drive"
  type        = string
  default     = "local-lvm"
}

variable "snippet_datastore" {
  description = "Datastore (with snippets content) for cloud-init user-data"
  type        = string
  default     = "local"
}

variable "network_bridge" {
  description = "Proxmox bridge to attach VM NICs to"
  type        = string
  default     = "vmbr0"
}

# ─── Network / MicroDNS ──────────────────────────────────────────────────────

variable "search_domain" {
  description = "DNS search domain (used to build the FQDN)"
  type        = string
  default     = "g8.lo"
}

variable "microdns_base_url" {
  description = "MicroDNS REST API base URL (DHCP reservation + DNS)"
  type        = string
  default     = "http://192.168.8.252:8080/api/v1"
}

variable "dns_zone_id" {
  description = "MicroDNS zone ID for search_domain — used to delete the auto-created A record on destroy"
  type        = string
}

# ─── cloud-init ──────────────────────────────────────────────────────────────

variable "ci_user" {
  description = "Default cloud-init user (used by the built-in user-data template)"
  type        = string
  default     = "fedora"
}

variable "ci_ssh_public_keys" {
  description = "SSH public keys injected into the default user"
  type        = list(string)
  default     = []
}

variable "tags" {
  description = "Proxmox tags applied to every VM"
  type        = list(string)
  default     = ["terraform", "fedora"]
}

# ─── Blast-radius guardrails ─────────────────────────────────────────────────
# This module must never be able to touch a hand-created VM. Two independent
# layers enforce that: (1) every VM this module creates is placed in a
# dedicated Proxmox resource pool, and the API token used to run Terraform is
# ACL-scoped to *only* that pool on the Proxmox side — a create/update/destroy
# call for any vmid outside the pool is rejected by Proxmox itself, regardless
# of what this config says. (2) as defense-in-depth, every vm_id is validated
# to fall inside an explicitly reserved range, so a typo'd/reused id fails
# `terraform plan` before it ever reaches the API.

variable "pool_id" {
  description = "Proxmox resource pool every VM is placed in (the automation's API token must be ACL-scoped to exactly this pool)"
  type        = string
  default     = "terraform-managed"
}

variable "vm_id_min" {
  description = "Lowest vm_id this module is allowed to manage"
  type        = number
  default     = 2000
}

variable "vm_id_max" {
  description = "Highest vm_id this module is allowed to manage"
  type        = number
  default     = 2100
}

# ─── VM fleet ────────────────────────────────────────────────────────────────
# Each VM gets a fixed MAC. A MicroDNS DHCP reservation pins MAC -> IP and
# auto-registers DNS; the VM boots via DHCP and receives the reserved IP.
# Set `user_data` to supply a fully-rendered cloud-config; otherwise the module's
# built-in template is used (hostname, default user + keys, qemu-guest-agent).

variable "vms" {
  description = "Map of VMs to create (key = short hostname)"
  type = map(object({
    vm_id     = number
    mac       = string
    ip        = string
    cores     = optional(number, 2)
    memory    = optional(number, 2048)
    disk_size = optional(number, 20)
    user_data = optional(string)
  }))

  validation {
    condition     = alltrue([for v in var.vms : v.vm_id >= var.vm_id_min && v.vm_id <= var.vm_id_max])
    error_message = "Every vm_id must be within [var.vm_id_min, var.vm_id_max] (${var.vm_id_min}-${var.vm_id_max} by default). This range is reserved for this module; hand-created VMs must never use it, and this module must never be pointed at a vm_id outside it."
  }
}
