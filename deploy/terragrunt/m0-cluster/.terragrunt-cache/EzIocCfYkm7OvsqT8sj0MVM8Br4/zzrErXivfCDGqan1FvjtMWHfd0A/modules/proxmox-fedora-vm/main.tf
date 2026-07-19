# ─── MicroDNS DHCP reservation (pins MAC -> IP, auto-registers DNS) ───────────
# Created before the VM boots so DHCP hands out the reserved IP. On destroy it
# removes the reservation AND the auto-created A record (the PTR cascades),
# since deleting a reservation does not cascade to DNS.

resource "terraform_data" "dns_reservation" {
  for_each = var.vms

  triggers_replace = {
    mac      = lower(each.value.mac)
    ip       = each.value.ip
    hostname = each.key
    base_url = var.microdns_base_url
    zone_id  = var.dns_zone_id
  }

  provisioner "local-exec" {
    command = <<-EOT
      curl -fsS -X POST '${self.triggers_replace.base_url}/dhcp/reservations' \
        -H 'Content-Type: application/json' \
        -d '{"mac":"${self.triggers_replace.mac}","ip":"${self.triggers_replace.ip}","hostname":"${self.triggers_replace.hostname}"}' \
        || curl -fsS -X PATCH '${self.triggers_replace.base_url}/dhcp/reservations/${self.triggers_replace.mac}' \
             -H 'Content-Type: application/json' \
             -d '{"ip":"${self.triggers_replace.ip}","hostname":"${self.triggers_replace.hostname}"}'
    EOT
  }

  provisioner "local-exec" {
    when    = destroy
    command = <<-EOT
      curl -fsS -X DELETE '${self.triggers_replace.base_url}/dhcp/reservations/${self.triggers_replace.mac}' || true
      RID=$(curl -fsS '${self.triggers_replace.base_url}/zones/${self.triggers_replace.zone_id}/records?limit=500' \
        | python3 -c "import sys,json; d=json.load(sys.stdin); recs=d.get('records',d) if isinstance(d,dict) else d; print(next((r['id'] for r in recs if r.get('name')=='${self.triggers_replace.hostname}' and isinstance(r.get('data'),dict) and r['data'].get('type')=='A'),''))")
      [ -n "$RID" ] && curl -fsS -X DELETE '${self.triggers_replace.base_url}/zones/${self.triggers_replace.zone_id}/records/'"$RID" || true
    EOT
  }
}

# ─── cloud-init user-data (one snippet per VM) ───────────────────────────────
# Use the caller-supplied user_data if present, otherwise the built-in template.

resource "proxmox_virtual_environment_file" "user_data" {
  for_each = var.vms

  content_type = "snippets"
  datastore_id = var.snippet_datastore
  node_name    = var.node_name

  source_raw {
    file_name = "${each.key}-user-data.yaml"
    data = coalesce(each.value.user_data, templatefile("${path.module}/templates/user-data.yaml.tftpl", {
      hostname = each.key
      fqdn     = "${each.key}.${var.search_domain}"
      ci_user  = var.ci_user
      ssh_keys = [for k in var.ci_ssh_public_keys : trimspace(k)]
    }))
  }
}

# ─── Fedora VMs ──────────────────────────────────────────────────────────────

resource "proxmox_virtual_environment_vm" "vm" {
  for_each = var.vms

  name      = "${each.key}.${var.search_domain}"
  node_name = var.node_name
  vm_id     = each.value.vm_id
  pool_id   = var.pool_id
  tags      = var.tags

  depends_on = [terraform_data.dns_reservation]

  agent {
    enabled = true
  }

  cpu {
    cores = each.value.cores
    type  = "host"
  }

  memory {
    dedicated = each.value.memory
  }

  scsi_hardware = "virtio-scsi-single"

  disk {
    datastore_id = var.vm_datastore
    import_from  = var.fedora_image
    interface    = "scsi0"
    size         = each.value.disk_size
    discard      = "on"
    ssd          = true
  }

  network_device {
    bridge      = var.network_bridge
    mac_address = each.value.mac
  }

  operating_system {
    type = "l26"
  }

  initialization {
    datastore_id = var.vm_datastore
    interface    = "ide2"

    # DHCP: the MicroDNS reservation supplies IP, gateway, DNS and domain.
    ip_config {
      ipv4 {
        address = "dhcp"
      }
    }

    user_data_file_id = proxmox_virtual_environment_file.user_data[each.key].id
  }

  lifecycle {
    ignore_changes = [
      disk[0].import_from, # avoid re-import churn after first create
    ]
  }
}
