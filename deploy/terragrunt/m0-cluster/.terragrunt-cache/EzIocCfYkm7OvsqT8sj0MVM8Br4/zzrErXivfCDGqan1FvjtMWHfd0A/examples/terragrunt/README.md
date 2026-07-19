# Terragrunt example

Shows how a project consumes the modules with **zero copied boilerplate**.
`root.hcl` defines the Proxmox provider, version constraints, and remote state
once; each unit (`g8/bits/`) just includes it, points at a module, and supplies
inputs.

## Layout

```
root.hcl                 # provider + versions + state — defined ONCE
g8/
  bits/terragrunt.hcl    # one fleet: includes root, sources the module, sets inputs
```

Add more units (`g8/web/`, `gw/k8s/`, …) as sibling directories — each is ~15
lines. Bump the module `?ref=` to roll out a fix everywhere on your own schedule.

## Run

```bash
export PROXMOX_API_TOKEN='root@pam!terraform=xxxxxxxx-....'   # never commit this
cd g8/bits
terragrunt init
terragrunt plan
terragrunt apply
```

`terragrunt run-all plan` / `apply` from the `terragrunt/` root operates on every
unit at once.

## What you no longer copy per project

- `provider "proxmox"` block (endpoint, token wiring, SSH key)
- `required_providers` / `required_version`
- Remote state backend config
- The VM + DHCP-reservation + DNS-cleanup logic (it's in the module)
