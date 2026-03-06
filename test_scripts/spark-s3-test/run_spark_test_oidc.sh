#!/bin/bash
set -e

cd "$(dirname "$0")"

echo "============================================="
echo "Spark S3 Integration Test Runner (OIDC+STS)"
echo "============================================="

# Cleanup function
cleanup() {
    echo "Cleaning up..."
    docker compose -f docker-compose-oidc.yml logs s3-server | tail -30
}
trap cleanup EXIT

echo "Step 1: Restarting cluster with OIDC enabled..."
# docker compose -f docker-compose-oidc.yml down -v 2>/dev/null || true
docker compose -f docker-compose-oidc.yml up -d --build

echo "Waiting for S3 server and Mock OIDC..."
sleep 10
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if curl -s http://localhost:9000 >/dev/null 2>&1; then
        echo "S3 server is ready!"
        break
    fi
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "ERROR: S3 server failed to start"
    docker compose -f docker-compose-oidc.yml logs s3-server
    exit 1
fi

echo "Waiting for Spark master..."
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if curl -s http://localhost:8088 >/dev/null 2>&1; then
        echo "Spark master is ready!"
        break
    fi
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "ERROR: Spark master failed to start"
    exit 1
fi

sleep 5

echo "Step 2: Authenticate and get STS token..."
# Fetch OIDC JWT Token
JWT_TOKEN=$(curl -s http://localhost:8080/token)
echo "JWT Token retrieved. Length: ${#JWT_TOKEN}"

# Exchange for STS Credentials
STS_XML=$(curl -s "http://localhost:9000/?Action=AssumeRoleWithWebIdentity&DurationSeconds=3600&RoleArn=arn:dfs:iam:::role/spark-job-role&RoleSessionName=spark-job-1&WebIdentityToken=${JWT_TOKEN}")

AK=$(echo "$STS_XML" | sed -n 's/.*<AccessKeyId>\(.*\)<\/AccessKeyId>.*/\1/p')
SK=$(echo "$STS_XML" | sed -n 's/.*<SecretAccessKey>\(.*\)<\/SecretAccessKey>.*/\1/p')
ST=$(echo "$STS_XML" | sed -n 's/.*<SessionToken>\(.*\)<\/SessionToken>.*/\1/p')

if [ -z "$AK" ] || [ -z "$SK" ] || [ -z "$ST" ]; then
    echo "ERROR: Failed to retrieve STS credentials."
    echo "Response: $STS_XML"
    docker compose -f docker-compose-oidc.yml logs s3-server
    exit 1
fi

echo "STS Credentials Acquired: $AK"

echo "Step 3: Creating S3 bucket using STS credentials..."
python3 << EOF
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9000",
    aws_access_key_id="$AK",
    aws_secret_access_key="$SK",
    aws_session_token="$ST",
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
EOF

echo "Step 4: Ensuring required Spark dependencies exist..."
docker compose -f docker-compose-oidc.yml exec -T spark-submit bash -c "mkdir -p /app/deps"
docker compose -f docker-compose-oidc.yml exec -T spark-submit bash -c "if [ ! -f /app/deps/hadoop-aws-3.3.4.jar ]; then wget -qO /app/deps/hadoop-aws-3.3.4.jar https://repo1.maven.org/maven2/org/apache/hadoop/hadoop-aws/3.3.4/hadoop-aws-3.3.4.jar; fi"
docker compose -f docker-compose-oidc.yml exec -T spark-submit bash -c "if [ ! -f /app/deps/aws-java-sdk-bundle-1.12.262.jar ]; then wget -qO /app/deps/aws-java-sdk-bundle-1.12.262.jar https://repo1.maven.org/maven2/com/amazonaws/aws-java-sdk-bundle/1.12.262/aws-java-sdk-bundle-1.12.262.jar; fi"

echo "Step 5: Running Spark S3 integration test with STS SessionToken..."
# AWS SessionProvider configuration added for STS token injection into Hadoop
docker compose -f docker-compose-oidc.yml exec -T spark-submit /opt/spark/bin/spark-submit \
    --master local[*] \
    --jars /app/deps/hadoop-aws-3.3.4.jar,/app/deps/aws-java-sdk-bundle-1.12.262.jar \
    --conf "spark.hadoop.fs.s3a.endpoint=http://s3-server:9000" \
    --conf "spark.hadoop.fs.s3a.access.key=$AK" \
    --conf "spark.hadoop.fs.s3a.secret.key=$SK" \
    --conf "spark.hadoop.fs.s3a.session.token=$ST" \
    --conf "spark.hadoop.fs.s3a.aws.credentials.provider=org.apache.hadoop.fs.s3a.TemporaryAWSCredentialsProvider" \
    --conf "spark.hadoop.fs.s3a.path.style.access=true" \
    --conf "spark.hadoop.fs.s3a.impl=org.apache.hadoop.fs.s3a.S3AFileSystem" \
    --conf "spark.hadoop.fs.s3a.connection.ssl.enabled=false" \
    --conf "spark.hadoop.fs.s3a.multiobjectdelete.enable=false" \
    --conf "spark.hadoop.mapreduce.fileoutputcommitter.algorithm.version=2" \
    --conf "spark.hadoop.fs.s3a.payload.signing.enabled=false" \
    --conf "spark.hadoop.fs.s3a.fast.upload=true" \
    /app/spark_s3_test.py

EXIT_CODE=$?

if [ $EXIT_CODE -eq 0 ]; then
    echo "============================================="
    echo "OIDC/STS + SPARK S3 INTEGRATION TEST PASSED!"
    echo "============================================="
else
    echo "============================================="
    echo "SPARK S3 INTEGRATION TEST FAILED!"
    echo "============================================="
fi

if [ $EXIT_CODE -ne 0 ]; then
    echo "Test failed. Dumping bucket contents for debugging..."
    python3 -c "
import boto3, os
s3 = boto3.client('s3',
    endpoint_url='http://localhost:9000',
    aws_access_key_id='$AK',
    aws_secret_access_key='$SK',
    aws_session_token='$ST',
    region_name='us-east-1'
)
try:
    response = s3.list_objects_v2(Bucket='spark-test-bucket')
    if 'Contents' in response:
        for obj in response['Contents']:
            print(f\"File: {obj['Key']}, Size: {obj['Size']}, ETag: {obj['ETag']}\")
            if 'part-' in obj['Key']:
                try:
                    res = s3.get_object(Bucket='spark-test-bucket', Key=obj['Key'])
                    print(f\"Content of {obj['Key']}:\")
                    print(res['Body'].read().decode('utf-8'))
                except Exception as e:
                    print(f\"Failed to read {obj['Key']}: {e}\")
    else:
        print('Bucket is empty!')
except Exception as e:
    print(f'Failed to list bucket: {e}')
"
fi

# Cleanup: Stop and remove all containers to prevent conflicts with other tests
echo ""
echo "Cleaning up docker-compose containers..."
docker compose -f docker-compose-oidc.yml down -v

exit $EXIT_CODE
