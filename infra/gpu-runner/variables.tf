# GPU CI runner — variables. NOT applied (S7 is "GPU-ready, GPU-off"); see README.md.

variable "region" {
  description = "AWS region for the GPU runner."
  type        = string
  default     = "us-east-1"
}

variable "instance_type" {
  description = "GPU instance type. g4dn.xlarge is one NVIDIA T4 — the cheapest CUDA GPU that runs the determinism gate."
  type        = string
  default     = "g4dn.xlarge"
}

variable "use_spot" {
  description = "Use a spot instance so an idle GPU is not an idle bill."
  type        = bool
  default     = true
}

variable "github_repo" {
  description = "owner/repo the runner registers against."
  type        = string
}

variable "github_runner_token" {
  description = "Short-lived self-hosted-runner registration token. Passed at apply time; never committed."
  type        = string
  sensitive   = true
}

variable "runner_labels" {
  description = "Labels the CI GPU jobs select on (runs-on: [self-hosted, gpu])."
  type        = string
  default     = "gpu,cuda"
}
