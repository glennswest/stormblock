output "vms" {
  description = "Created VMs: hostname -> {vm_id, fqdn, mac, ip}"
  value = {
    for k, v in var.vms : k => {
      vm_id = v.vm_id
      fqdn  = "${k}.${var.search_domain}"
      mac   = lower(v.mac)
      ip    = v.ip
    }
  }
}

output "ssh_targets" {
  description = "Convenience SSH commands (default user)"
  value       = [for k, v in var.vms : "ssh ${var.ci_user}@${k}.${var.search_domain}"]
}
