# Unit: m0-cluster — M0 milestone harness (issue #1): 3 stormblock storage
# nodes + 1 initiator host. The initiator attaches each node's volume over
# NVMe-oF/TCP with the kernel nvme-tcp initiator and runs fio baselines.
#
# Throwaway by design: apply, run deploy/m0/run-m0-baseline.sh, destroy,
# release the vmids (../free-vmid.sh --release <id>).
#
# vm_ids allocated live via ../free-vmid.sh and passed in:
#   M0_VMIDS="2012,2013,2014,2015" terragrunt apply

include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.3.0"
}

locals {
  ssh_key = trimspace(file(pathexpand("~/.ssh/id_rsa.pub")))
  vmids   = [for s in split(",", get_env("M0_VMIDS", "0,0,0,0")) : tonumber(s)]

  # stormblock release the nodes install — released artifacts only, no build
  # on the node (same convention as rustkube).
  stormblock_url = "https://github.com/glennswest/stormblock/releases/download/v6.1.1/stormblock-x86_64-linux-musl"

  # Fixed MAC -> reserved IP outside the g8 DHCP pool (.100-.200);
  # .62-.65 free per sister-project scan 2026-07-19.
  storm_nodes = {
    storm1 = { idx = 0, mac = "BC:24:11:08:00:62", ip = "192.168.8.62" }
    storm2 = { idx = 1, mac = "BC:24:11:08:00:63", ip = "192.168.8.63" }
    storm3 = { idx = 2, mac = "BC:24:11:08:00:64", ip = "192.168.8.64" }
  }
  initiator = { idx = 3, mac = "BC:24:11:08:00:65", ip = "192.168.8.65" }
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [local.ssh_key]
  tags               = ["terraform", "fedora", "stormblock", "m0", "throwaway"]

  vm_datastore      = "test-lvm-thin"
  snippet_datastore = "terraform-snippets"

  vms = merge(
    {
      for name, n in local.storm_nodes : name => {
        vm_id     = local.vmids[n.idx]
        mac       = n.mac
        ip        = n.ip
        cores     = 2
        memory    = 2048
        disk_size = 32
        user_data = templatefile("${get_terragrunt_dir()}/templates/storm-user-data.yaml.tftpl", {
          hostname       = name
          fqdn           = "${name}.g8.lo"
          ssh_keys       = [local.ssh_key]
          node_ip        = n.ip
          stormblock_url = local.stormblock_url
          nqn            = "nqn.2026-01.io.stormblock:${name}"
        })
      }
    },
    {
      m0init = {
        vm_id     = local.vmids[local.initiator.idx]
        mac       = local.initiator.mac
        ip        = local.initiator.ip
        cores     = 2
        memory    = 2048
        disk_size = 20
        user_data = templatefile("${get_terragrunt_dir()}/templates/initiator-user-data.yaml.tftpl", {
          hostname = "m0init"
          fqdn     = "m0init.g8.lo"
          ssh_keys = [local.ssh_key]
        })
      }
    }
  )
}
