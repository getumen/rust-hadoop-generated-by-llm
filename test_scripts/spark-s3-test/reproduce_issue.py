import boto3
from botocore.config import Config
import time

s3 = boto3.client(
    "s3",
    endpoint_url="http://localhost:9000",
    aws_access_key_id="dummy",
    aws_secret_access_key="dummy",
    region_name="us-east-1",
    config=Config(s3={"addressing_style": "path"})
)

bucket_name = "spark-test-bucket"
print(f"Attempting to create bucket: {bucket_name}")
try:
    s3.create_bucket(Bucket=bucket_name)
    print(f"Bucket {bucket_name} created successfully!")
except Exception as e:
    print(f"Error creating bucket: {e}")
