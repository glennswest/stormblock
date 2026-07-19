# Unit: g8 "bits" fleet — bit1..bit4.g8.lo
#
# This whole file is what a consuming project needs: include the root (provider +
# versions + state), point at the module, and pass inputs. No copied boilerplate.

include "root" {
  path = find_in_parent_folders("root.hcl")
}

terraform {
  # In-repo path for this example. From another repo you'd use:
  #   source = "git::ssh://git@github.com/glennswest/terraform-modules.git//modules/proxmox-fedora-vm?ref=v0.1.0"
  source = "${get_repo_root()}/modules/proxmox-fedora-vm"
}

inputs = {
  dns_zone_id        = "9bed60c8-1664-4183-88f9-a1a21b927edc" # g8.lo
  ci_ssh_public_keys = [file(pathexpand("~/.ssh/id_rsa.pub"))]
  tags               = ["terraform", "fedora", "bit"]

  vms = {
    bit1 = { vm_id = 141, mac = "BC:24:11:08:00:01", ip = "192.168.8.91" }
    bit2 = { vm_id = 142, mac = "BC:24:11:08:00:02", ip = "192.168.8.92" }
    bit3 = { vm_id = 143, mac = "BC:24:11:08:00:03", ip = "192.168.8.93" }
    bit4 = { vm_id = 144, mac = "BC:24:11:08:00:04", ip = "192.168.8.94" }
  }
}
