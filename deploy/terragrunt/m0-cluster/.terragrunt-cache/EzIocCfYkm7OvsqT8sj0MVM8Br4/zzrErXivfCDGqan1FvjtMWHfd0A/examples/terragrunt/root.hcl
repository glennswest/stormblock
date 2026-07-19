# Root Terragrunt config — included by every unit via:
#   include "root" { path = find_in_parent_folders("root.hcl") }
#
# This is where the per-project boilerplate lives ONCE: the Proxmox provider,
# Terraform version constraints, and remote state. Child units only declare
# `terraform { source = "...module..." }` and their inputs.

locals {
  # Proxmox endpoint + token. The token is read from the environment so it never
  # lands in code or state:  export PROXMOX_API_TOKEN='root@pam!terraform=...'
  proxmox_endpoint = "https://pve.g8.lo:8006/"
  ssh_private_key  = pathexpand("~/.ssh/id_rsa")
}

# ── Generate the provider config into every unit ──
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

# Version constraints (required_providers) come from the module's own
# versions.tf, so Terragrunt does not generate them here.

# ── Remote state ──
# Local for now. Swap for an S3-compatible backend (e.g. MinIO on the network)
# to share + lock state across people/projects — uncomment and fill in:
#
# remote_state {
#   backend = "s3"
#   generate = { path = "backend.tf", if_exists = "overwrite_terragrunt" }
#   config = {
#     bucket                      = "tfstate"
#     key                         = "${path_relative_to_include()}/terraform.tfstate"
#     endpoints                   = { s3 = "https://minio.g8.lo:9000" }
#     region                      = "us-east-1"
#     skip_credentials_validation = true
#     skip_region_validation      = true
#     skip_requesting_account_id  = true
#     use_path_style              = true
#   }
# }

# The token flows to every unit as a TF var:  export TF_VAR... not needed —
# Terragrunt injects it from the env var below.
inputs = {
  proxmox_api_token = get_env("PROXMOX_API_TOKEN")
}
