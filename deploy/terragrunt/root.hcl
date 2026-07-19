# Root Terragrunt config for stormblock test infrastructure.
# Included by every unit:  include "root" { path = find_in_parent_folders("root.hcl") }
#
# Provider + token wiring lives here ONCE. Units only declare their module
# `source` and `inputs`, referencing the shared, versioned module at
# github.com/glennswest/terraform-modules (always pin ?ref=<tag>). Same
# convention as ../rustkube and ../irondirectory.

locals {
  proxmox_endpoint = "https://pve.g8.lo:8006/"
  ssh_private_key  = pathexpand("~/.ssh/id_rsa")
}

# Token comes from the environment so it never lands in code or state:
#   export PROXMOX_API_TOKEN='terraform-svc@pve!rustkube=...'
# Source it from a gitignored .env — don't mint a fresh one; Proxmox never
# re-displays a token secret. Must be the dedicated, pool-scoped
# terraform-svc@pve service token — root@pam tokens are BANNED (see
# terraform-modules CLAUDE.md, "Incident: 2026-07-08").
generate "provider" {
  path      = "provider.tf"
  if_exists = "overwrite_terragrunt"
  contents  = <<-EOF
    provider "proxmox" {
      endpoint  = "${local.proxmox_endpoint}"
      api_token = var.proxmox_api_token
      insecure  = true
      ssh {
        agent       = false
        username    = "root"
        private_key = file("${local.ssh_private_key}")
      }
    }

    variable "proxmox_api_token" {
      type      = string
      sensitive = true
    }
  EOF
}

inputs = {
  proxmox_api_token = get_env("PROXMOX_API_TOKEN")
}
