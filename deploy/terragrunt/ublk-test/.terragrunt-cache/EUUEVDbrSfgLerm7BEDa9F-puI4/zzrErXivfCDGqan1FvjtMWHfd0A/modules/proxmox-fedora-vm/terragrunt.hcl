# Unit: ublktest — throwaway VM for stormblock ublk runtime verification
# (issue #12 end-to-end: boot-local → /dev/ublkb0 → mkfs/mount/verify).
#
# Throwaway by design: apply, run the test, destroy, release the vmid
# (../free-vmid.sh --release <id>). Not part of any long-lived environment.
#
# vm_id is allocated live via ../free-vmid.sh (range 2000-2100) and passed in:
#   export TF_VAR_unused=1  # (nothing else needed)
#   VMID=$(../free-vmid.sh) UBLKTEST_VMID=$VMID terragrunt apply

include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.3.0"
}

locals {
  ssh_key = trimspace(file(pathexpand("~/.ssh/id_rsa.pub")))
  # Fixed MAC -> reserved IP outside the g8 DHCP pool (.100-.200); .60 free
  # per sister-project scan 2026-07-19 (.61 = irondirectory).
  node = {
    vm_id = tonumber(get_env("UBLKTEST_VMID", "0"))
    mac   = "BC:24:11:08:00:60"
    ip    = "192.168.8.60"
  }
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [local.ssh_key]
  tags               = ["terraform", "fedora", "stormblock", "ublk-test", "throwaway"]

  vm_datastore      = "test-lvm-thin"
  snippet_datastore = "terraform-snippets"

  vms = {
    ublktest = {
      vm_id     = local.node.vm_id
      mac       = local.node.mac
      ip        = local.node.ip
      cores     = 2
      memory    = 2048
      disk_size = 20
      user_data = templatefile("${get_terragrunt_dir()}/templates/user-data.yaml.tftpl", {
        hostname = "ublktest"
        fqdn     = "ublktest.g8.lo"
        ci_user  = "fedora"
        ssh_keys = [local.ssh_key]
      })
    }
  }
}
