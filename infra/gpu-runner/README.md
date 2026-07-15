# The GPU CI runner — provisioning as code

**Status: not applied.** S7 ships "GPU-ready, GPU-off." This directory is the code to stand up a
self-hosted GPU CI runner (directive 2), reviewable and ready to `terraform apply` — but it has
**not been applied**, because this environment has no cloud credentials and no way to provision an
instance. Until this runner exists and a GPU CI job runs on it, the GPU path stays
disabled-by-default (the AVX-512 rule, device edition — [determinism contract §3](../../docs/DETERMINISM-CONTRACT.md)).

Standing this up is a **one-command, one-credential** operation, and that is the point: the work
that requires hardware and money is isolated to a `terraform apply` a maintainer with an account
can run, and nothing in the codebase pretends it has been done.

## What it provisions

A single spot/on-demand GPU instance (default: AWS `g4dn.xlarge`, one NVIDIA T4 — the cheapest
CUDA GPU that runs the determinism gate), registered as a GitHub Actions self-hosted runner with
the label `gpu`. The CI workflow's GPU jobs (`runs-on: [self-hosted, gpu]`) light up the moment a
runner with that label is online, and go back to "no runner, jobs skipped" when it is torn down.

- `main.tf` — the instance, its security group (egress only), and an IAM role scoped to nothing
  but what the runner agent needs. Spot by default, so an idle GPU is not an idle bill.
- `variables.tf` — region, instance type, the GitHub registration token, the repo. No secret is
  committed; the token is passed at apply time.
- `cloud-init.yaml` — installs the NVIDIA driver + CUDA toolkit, the Actions runner agent, and
  registers it with the `gpu` label. Idempotent.

## The apply, when someone has an account

```bash
cd infra/gpu-runner
terraform init
# A short-lived runner registration token from:
#   Settings -> Actions -> Runners -> New self-hosted runner
terraform apply -var="github_runner_token=$TOKEN" -var="github_repo=Bobcatsfan33/PrismDB"
```

Tear-down is `terraform destroy` — and because the GPU jobs are gated on the runner label, tearing
it down does not break `main`; the GPU jobs simply stop running, exactly as they do today.

## What graduates when it is applied

The moment a `gpu` runner is online and green:

1. The CUDA kernels (behind the `cuda` feature) are built and run in CI, and the **selection-identity
   gate** proves them equal to the `GpuReference` route on the golden, layout-variant, and
   boundary-tie corpora. Only then does `Route::Cuda` become selectable in a default build.
2. The **crossover cost model** is derived: the routing thresholds (`GPU_MIN_CANDIDATES` and its
   siblings), currently placeholders marked device-conditional and un-derived, get real receipts
   measured end-to-end on the runner (charter C-6).
3. The **fault-injection gate** runs against real CUDA errors (a real OOM, a real device reset),
   not just the reference's injected faults.

Until then, every one of those is tested against the CPU reference, and the sprint says so.
