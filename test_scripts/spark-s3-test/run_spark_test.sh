#!/bin/bash
set -e

# Change to the directory containing this script
cd "$(dirname "$0")"

echo "============================================="
echo "Spark S3 Integration Test Runner"
echo "============================================="

# Cleanup function
cleanup() {
    echo ""
    echo "Cleaning up..."
    docker-compose down -v 2>/dev/null || true
}

# Set trap to cleanup on exit
trap cleanup EXIT

# Step 1: Build and start the cluster
echo ""
echo "Step 1: Building and starting the cluster..."
docker-compose down -v 2>/dev/null || true
docker-compose up -d --build

# Step 2: Wait for services to be ready
echo ""
echo "Step 2: Waiting for services to be ready..."

# Wait for S3 server (port is exposed on host)
echo "Waiting for S3 server..."
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if curl -s http://localhost:9000 >/dev/null 2>&1 || nc -z localhost 9000 2>/dev/null; then
        echo "S3 server is ready!"
        break
    fi
    echo "Waiting... ($RETRIES)"
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "ERROR: S3 server failed to start"
    docker-compose logs s3-server
    exit 1
fi

# Wait for Spark master (port 8088 is exposed on host)
echo "Waiting for Spark master..."
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if curl -s http://localhost:8088 >/dev/null 2>&1; then
        echo "Spark master is ready!"
        break
    fi
    echo "Waiting... ($RETRIES)"
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "ERROR: Spark master failed to start"
    docker-compose logs spark-master
    exit 1
fi

# Wait for Spark worker to register
echo "Waiting for Spark worker to register..."
sleep 10

# Step 3: Create the S3 bucket using Python on host
echo ""
echo "Step 3: Creating S3 bucket..."
python3 << 'EOF'
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9000",
    aws_access_key_id="dummy",
    aws_secret_access_key="dummy",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"})
)

bucket_name = "spark-test-bucket"
try:
    s3.create_bucket(Bucket=bucket_name)
    print(f"Bucket {bucket_name} created successfully!")
except Exception as e:
    if "BucketAlreadyOwnedByYou" in str(e) or "409" in str(e):
        print(f"Bucket {bucket_name} already exists")
    else:
        print(f"Error creating bucket: {e}")
        raise e

# Verify bucket exists
response = s3.list_buckets()
buckets = [b["Name"] for b in response.get("Buckets", [])]
print(f"Available buckets: {buckets}")
EOF

# Step 4: Run Spark test
echo ""
echo "Step 4: Running Spark S3 integration test..."
docker-compose exec -T spark-submit /opt/spark/bin/spark-submit \
    --master spark://spark-master:7077 \
    --packages org.apache.hadoop:hadoop-aws:3.3.4,com.amazonaws:aws-java-sdk-bundle:1.12.262 \
    --conf "spark.hadoop.fs.s3a.endpoint=http://s3-server:9000" \
    --conf "spark.hadoop.fs.s3a.access.key=dummy" \
    --conf "spark.hadoop.fs.s3a.secret.key=dummy" \
    --conf "spark.hadoop.fs.s3a.path.style.access=true" \
    --conf "spark.hadoop.fs.s3a.impl=org.apache.hadoop.fs.s3a.S3AFileSystem" \
    --conf "spark.hadoop.fs.s3a.connection.ssl.enabled=false" \
    --conf "spark.hadoop.fs.s3a.aws.credentials.provider=org.apache.hadoop.fs.s3a.SimpleAWSCredentialsProvider" \
    --conf "spark.driver.extraJavaOptions=-Dcom.amazonaws.services.s3.enableV4=true" \
    --conf "spark.executor.extraJavaOptions=-Dcom.amazonaws.services.s3.enableV4=true" \
    /app/spark_s3_test.py

EXIT_CODE=$?

if [ $EXIT_CODE -eq 0 ]; then
    echo ""
    echo "============================================="
    echo "SPARK S3 INTEGRATION TEST PASSED!"
    echo "============================================="
else
    echo ""
    echo "============================================="
    echo "SPARK S3 INTEGRATION TEST FAILED!"
    echo "============================================="
    echo ""
    echo "S3 Server logs:"
    docker-compose logs s3-server | tail -50
fi

exit $EXIT_CODE
