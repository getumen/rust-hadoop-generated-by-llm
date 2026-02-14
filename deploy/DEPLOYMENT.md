# Deploying Rust Hadoop DFS on Kubernetes

This guide explains how to deploy the Rust Hadoop DFS cluster using the provided Helm chart.

## Quick Start Guides

- **[Rancher Desktop Quick Start](./RANCHER_DESKTOP_QUICKSTART.md)** - Recommended for local development (macOS/Linux/Windows)
- **Minikube** - See instructions below
- **Cloud Providers (EKS/GKE/AKS)** - See instructions below

## Prerequisites

- A Kubernetes cluster (Rancher Desktop, Minikube, Docker Desktop K8s, or a cloud provider like GKE/EKS/AKS).
- [Helm](https://helm.sh/docs/intro/install/) installed.

## Installation

1.  **Build the Docker image**:
    ```bash
    docker build -t rust-hadoop:latest .
    ```

2.  **Install the Helm chart**:
    ```bash
    helm install my-dfs ./deploy/helm/rust-hadoop
    ```

## Configuration

You can customize the deployment by modifying `values.yaml` or using `--set` flags.

| Parameter                  | Description                                    | Default |
| -------------------------- | ---------------------------------------------- | ------- |
| `metaserver.replicaCount`  | Number of Metaservers (should be odd for Raft) | `3`     |
| `chunkserver.replicaCount` | Number of ChunkServers                         | `3`     |
| `s3server.replicaCount`    | Number of S3 frontend servers                  | `1`     |

## Accessing the S3 API

Once deployed, you can access the S3 API via the service:
```bash
kubectl port-forward svc/my-dfs-s3server 9000:9000
```
Then use any S3 client (e.g., `aws cli`) with `--endpoint-url http://localhost:9000`.
