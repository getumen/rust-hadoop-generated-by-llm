# Rust Hadoop DFS on Rancher Desktop - クイックスタートガイド

このガイドでは、Rancher Desktopを使用してRust Hadoop DFSをローカルKubernetes環境にデプロイする手順を説明します。

## 前提条件

- [Rancher Desktop](https://rancherdesktop.io/)がインストールされていること
- Rancher DesktopでKubernetesが有効になっていること
- `kubectl`コマンドが利用可能であること
- `helm`コマンドがインストールされていること

## Rancher Desktopの設定

1. **Rancher Desktopを起動**
   - Rancher Desktopアプリケーションを開く
   - Kubernetes設定でKubernetesを有効化

2. **Container Runtimeの選択**
   - `containerd`または`dockerd (moby)`を選択
   - `dockerd`を推奨（Docker CLIとの互換性が高い）

3. **リソース設定**
   - メモリ: 最低4GB、推奨8GB以上
   - CPU: 最低2コア、推奨4コア以上
   - ディスク: 最低20GB

## デプロイ手順

### 1. Dockerイメージのビルド

Rancher DesktopのKubernetesは、ローカルのDockerイメージを直接使用できます。

```bash
# プロジェクトルートディレクトリで実行
docker build -t rust-hadoop:latest .
```

ビルドには数分かかります。完了後、イメージが作成されたことを確認:

```bash
docker images | grep rust-hadoop
```

### 2. Helmチャートのデプロイ

```bash
# デフォルト設定でデプロイ
helm install my-dfs ./deploy/helm/rust-hadoop

# または、カスタム設定でデプロイ
helm install my-dfs ./deploy/helm/rust-hadoop \
  --set metaserver.replicaCount=3 \
  --set chunkserver.replicaCount=3 \
  --set s3server.replicaCount=1
```

### 3. デプロイ状態の確認

```bash
# Pod一覧を確認
kubectl get pods

# すべてのリソースを確認
kubectl get all

# Pod詳細を確認
kubectl describe pod <pod-name>

# Podログを確認
kubectl logs <pod-name>
```

Podが`Running`状態になるまで待ちます（初回は数分かかることがあります）。

期待される出力:
```
NAME                          READY   STATUS    RESTARTS   AGE
my-dfs-chunkserver-0          1/1     Running   0          2m
my-dfs-chunkserver-1          1/1     Running   0          2m
my-dfs-chunkserver-2          1/1     Running   0          2m
my-dfs-metaserver-0           1/1     Running   0          2m
my-dfs-metaserver-1           1/1     Running   0          2m
my-dfs-metaserver-2           1/1     Running   0          2m
my-dfs-s3server-xxxxx         1/1     Running   0          2m
```

### 4. S3 APIへのアクセス

Port-forwardingを使用してS3 APIにアクセスします。

```bash
# S3 APIをローカルポート9000に公開
kubectl port-forward svc/my-dfs-s3server 9000:9000
```

別のターミナルで、AWS CLIを使用してアクセス:

```bash
# バケット作成
aws s3 mb s3://test-bucket --endpoint-url http://localhost:9000

# ファイルアップロード
echo "Hello Rust Hadoop DFS" > test.txt
aws s3 cp test.txt s3://test-bucket/ --endpoint-url http://localhost:9000

# バケット一覧
aws s3 ls --endpoint-url http://localhost:9000

# ファイル一覧
aws s3 ls s3://test-bucket/ --endpoint-url http://localhost:9000

# ファイルダウンロード
aws s3 cp s3://test-bucket/test.txt downloaded.txt --endpoint-url http://localhost:9000
```

## Persistent Volumeについて

Rancher Desktopは`local-path`というStorageClass provisioenerをデフォルトで提供しています。

```bash
# StorageClassを確認
kubectl get storageclass

# PersistentVolumeClaimを確認
kubectl get pvc

# PersistentVolumeを確認
kubectl get pv
```

データは以下のディレクトリに保存されます:
- **macOS**: `/Users/<username>/.local/share/rancher-desktop/k3s/storage/`
- **Linux**: `~/.local/share/rancher-desktop/k3s/storage/`
- **Windows**: `C:\Users\<username>\.local\share\rancher-desktop\k3s\storage\`

## モニタリングとデバッグ

### Metaserver (Raft)の状態確認

```bash
# Metaserver-0のRaft状態を確認
kubectl port-forward my-dfs-metaserver-0 8080:8080

# 別のターミナルで
curl http://localhost:8080/health
curl http://localhost:8080/raft/state
```

### ログの確認

```bash
# 特定のPodのログを表示
kubectl logs my-dfs-metaserver-0

# リアルタイムでログを表示
kubectl logs -f my-dfs-metaserver-0

# 前回のコンテナのログを表示（クラッシュ時のデバッグ用）
kubectl logs my-dfs-metaserver-0 --previous
```

### Podへの接続

```bash
# Podに対話的にアクセス
kubectl exec -it my-dfs-metaserver-0 -- /bin/bash

# 単一コマンドを実行
kubectl exec my-dfs-metaserver-0 -- ls -la /data
```

## スケーリング

### ChunkServerのスケールアウト

```bash
# ChunkServerを5台に増やす
helm upgrade my-dfs ./deploy/helm/rust-hadoop --set chunkserver.replicaCount=5

# スケール確認
kubectl get pods -l component=chunkserver
```

### S3 Serverのスケールアウト

```bash
# S3 Serverを3台に増やす
helm upgrade my-dfs ./deploy/helm/rust-hadoop --set s3server.replicaCount=3

# スケール確認
kubectl get pods -l component=s3server
```

## アップグレード

### ローリングアップデート

```bash
# Dockerイメージを再ビルド
docker build -t rust-hadoop:v1.1.0 .

# values.yamlを更新（imageタグを変更）
helm upgrade my-dfs ./deploy/helm/rust-hadoop --set image.tag=v1.1.0

# ロールアウト状態を確認
kubectl rollout status statefulset/my-dfs-metaserver
kubectl rollout status statefulset/my-dfs-chunkserver
kubectl rollout status deployment/my-dfs-s3server
```

## トラブルシューティング

### Podが起動しない

```bash
# Pod詳細を確認
kubectl describe pod <pod-name>

# イベントを確認
kubectl get events --sort-by='.lastTimestamp'
```

**よくある原因**:
- イメージがビルドされていない → Dockerイメージを再ビルド
- リソース不足 → Rancher Desktopのメモリ/CPU設定を増やす
- PVC作成失敗 → `kubectl get pvc`でステータスを確認

### Metaserver間の通信エラー

```bash
# Headless Serviceが正常か確認
kubectl get svc my-dfs-metaserver-headless

# DNS解決を確認
kubectl run -it --rm debug --image=busybox --restart=Never -- nslookup my-dfs-metaserver-headless
```

### データが消えた

Rancher Desktopを再起動すると、PersistentVolumeのデータは保持されますが、Kubernetesクラスタをリセットすると削除されます。

```bash
# PVを確認
kubectl get pv

# データのバックアップ（例: metaserver-0）
kubectl cp my-dfs-metaserver-0:/data ./backup-metaserver-0
```

## クリーンアップ

### Helmリリースの削除

```bash
# Helm releaseを削除
helm uninstall my-dfs

# PVCを削除（データも削除されます）
kubectl delete pvc -l app.kubernetes.io/instance=my-dfs
```

### Dockerイメージの削除

```bash
docker rmi rust-hadoop:latest
```

## 高度な設定

### カスタムStorageClassの使用

```bash
# カスタムStorageClassでデプロイ
helm install my-dfs ./deploy/helm/rust-hadoop \
  --set metaserver.storage.storageClassName=local-path \
  --set chunkserver.storage.storageClassName=local-path
```

### リソース制限の設定

```bash
helm install my-dfs ./deploy/helm/rust-hadoop \
  --set metaserver.resources.requests.cpu=500m \
  --set metaserver.resources.requests.memory=512Mi \
  --set metaserver.resources.limits.cpu=1000m \
  --set metaserver.resources.limits.memory=1Gi \
  --set chunkserver.resources.requests.cpu=500m \
  --set chunkserver.resources.requests.memory=1Gi \
  --set chunkserver.resources.limits.cpu=2000m \
  --set chunkserver.resources.limits.memory=4Gi
```

### カスタムvalues.yamlの使用

`values-rancher.yaml`を作成:

```yaml
image:
  repository: rust-hadoop
  tag: latest
  pullPolicy: IfNotPresent

metaserver:
  replicaCount: 3
  storage:
    size: 2Gi
    storageClassName: local-path
  resources:
    requests:
      cpu: 500m
      memory: 512Mi
    limits:
      cpu: 1000m
      memory: 1Gi

chunkserver:
  replicaCount: 3
  storage:
    size: 20Gi
    storageClassName: local-path
  resources:
    requests:
      cpu: 500m
      memory: 1Gi
    limits:
      cpu: 2000m
      memory: 4Gi

s3server:
  replicaCount: 2
  resources:
    requests:
      cpu: 200m
      memory: 256Mi
    limits:
      cpu: 500m
      memory: 512Mi
```

デプロイ:

```bash
helm install my-dfs ./deploy/helm/rust-hadoop -f values-rancher.yaml
```

## 参考情報

- [Rancher Desktop公式ドキュメント](https://docs.rancherdesktop.io/)
- [Kubernetes公式ドキュメント](https://kubernetes.io/docs/)
- [Helm公式ドキュメント](https://helm.sh/docs/)
- [本プロジェクトのREADME](../README.md)

## よくある質問

**Q: Rancher DesktopとDocker Desktopの違いは?**
A: Rancher DesktopはKubernetesをネイティブサポートし、軽量で無料です。Docker Desktopは有料版の機能が多いです。

**Q: Minikubeでも動作しますか?**
A: はい、Minikubeでも同様の手順でデプロイできます。StorageClassは`standard`を使用してください。

**Q: productionで使用できますか?**
A: Rancher Desktopはローカル開発環境向けです。本番環境ではEKS、GKE、AKSなどのマネージドKubernetesサービスを使用してください。

**Q: Ingressを使ってS3 APIを公開できますか?**
A: はい、Nginx IngressやTraefikを使用してS3 APIを公開できます。詳細はHelm chartのドキュメントを参照してください。
