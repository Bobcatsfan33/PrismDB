# GPU CI runner — a single self-hosted GitHub Actions runner on a CUDA GPU instance.
# NOT applied. See README.md: this is the reviewable provisioning code, ready to `terraform apply`
# for a maintainer with a cloud account. Until applied, the GPU CI jobs have no runner and are
# skipped, and the GPU path ships disabled-by-default (determinism contract §3).

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region
}

# The most recent Ubuntu 22.04 LTS AMI — the driver + CUDA toolkit are installed by cloud-init, so
# a plain base image keeps the code portable rather than pinning a vendor GPU AMI.
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*"]
  }
}

resource "aws_security_group" "runner" {
  name_prefix = "prism-gpu-runner-"
  description = "Egress only: the runner reaches out to GitHub; nothing reaches in."
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_instance" "runner" {
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  vpc_security_group_ids = [aws_security_group.runner.id]

  dynamic "instance_market_options" {
    for_each = var.use_spot ? [1] : []
    content {
      market_type = "spot"
    }
  }

  # The GPU disk: CUDA + the driver need room.
  root_block_device {
    volume_size = 60
    volume_type = "gp3"
  }

  user_data = templatefile("${path.module}/cloud-init.yaml", {
    github_repo   = var.github_repo
    runner_token  = var.github_runner_token
    runner_labels = var.runner_labels
  })

  tags = {
    Name    = "prism-gpu-ci-runner"
    Purpose = "PrismDB S7 GPU determinism gate"
  }
}

output "runner_public_ip" {
  value       = aws_instance.runner.public_ip
  description = "For diagnostics only; the security group allows no inbound."
}
