import boto3
import botocore
import hashlib
import binascii
import uuid
import sys
import os

# Configuration
ENDPOINT_URL = os.environ.get("S3_ENDPOINT", "http://localhost:9000")
AWS_ACCESS_KEY_ID = "dummy"
AWS_SECRET_ACCESS_KEY = "dummy"
REGION_NAME = "us-east-1"
BUCKET_NAME = f"checksum-test-{uuid.uuid4().hex[:8]}"

def get_s3_client():
    return boto3.client(
        "s3",
        endpoint_url=ENDPOINT_URL,
        aws_access_key_id=AWS_ACCESS_KEY_ID,
        aws_secret_access_key=AWS_SECRET_ACCESS_KEY,
        region_name=REGION_NAME,
        config=botocore.config.Config(s3={'addressing_style': 'path'})
    )

def calculate_mpu_etag(parts_data):
    """Calculates the expected AWS MPU ETag: md5(concatenated_binary_md5s)-N"""
    combined_md5_bin = b""
    for data in parts_data:
        combined_md5_bin += hashlib.md5(data).digest()
    final_hash = hashlib.md5(combined_md5_bin).hexdigest()
    return f'"{final_hash}-{len(parts_data)}"'

def test_put_object_with_checksum(s3):
    print("--- Test: PutObject with CRC32C ---")
    key = "crc32c-test.txt"
    content = b"Data with CRC32C integrity check."

    # Upload with CRC32C
    print(f"Uploading {key} with ChecksumAlgorithm='CRC32C'")
    # Note: Boto3 handles the actual calculation if we just specify the algorithm
    s3.put_object(
        Bucket=BUCKET_NAME,
        Key=key,
        Body=content,
        ChecksumAlgorithm='CRC32C'
    )

    # Verify via HeadObject
    print("Verifying ETag and Metadata via HeadObject")
    resp = s3.head_object(Bucket=BUCKET_NAME, Key=key)

    # Calculate expected MD5 ETag
    expected_etag = f'"{hashlib.md5(content).hexdigest()}"'
    actual_etag = resp['ETag']

    print(f"Expected ETag: {expected_etag}")
    print(f"Actual ETag:   {actual_etag}")
    assert actual_etag == expected_etag, "ETag mismatch for PutObject"

    # Verify LastModified is a valid datetime
    print(f"LastModified: {resp['LastModified']}")
    assert resp['LastModified'] is not None, "LastModified missing"
    print("PutObject with Checksum: SUCCESS")

def test_multipart_upload_checksum(s3):
    print("\n--- Test: Multipart Upload ETag Calculation ---")
    key = "mpu-checksum.dat"
    # Create 3 parts
    p1 = b"Part 1 data - " * 1000
    p2 = b"Part 2 data - " * 2000
    p3 = b"Part 3 data - " * 500
    parts_data = [p1, p2, p3]

    # Initiate
    print("Initiating MPU")
    mpu = s3.create_multipart_upload(Bucket=BUCKET_NAME, Key=key)
    upload_id = mpu['UploadId']

    try:
        completed_parts = []
        for i, data in enumerate(parts_data, 1):
            print(f"Uploading Part {i} ({len(data)} bytes)")
            resp = s3.upload_part(
                Bucket=BUCKET_NAME, Key=key,
                PartNumber=i, UploadId=upload_id, Body=data
            )
            completed_parts.append({'PartNumber': i, 'ETag': resp['ETag']})

        # Complete
        print("Completing MPU")
        s3.complete_multipart_upload(
            Bucket=BUCKET_NAME, Key=key, UploadId=upload_id,
            MultipartUpload={'Parts': completed_parts}
        )

        # Verify MPU ETag
        resp = s3.head_object(Bucket=BUCKET_NAME, Key=key)
        actual_etag = resp['ETag']
        expected_etag = calculate_mpu_etag(parts_data)

        print(f"Expected MPU ETag: {expected_etag}")
        print(f"Actual MPU ETag:   {actual_etag}")
        assert actual_etag == expected_etag, "MPU ETag mismatch (MD5 aggregation failed)"
        print("Multipart Upload Checksum: SUCCESS")

    except Exception as e:
        s3.abort_multipart_upload(Bucket=BUCKET_NAME, Key=key, UploadId=upload_id)
        raise e

def test_copy_object_metadata(s3):
    print("\n--- Test: CopyObject ETag Propagation ---")
    src_key = "source-obj.txt"
    dest_key = "dest-obj.txt"
    content = b"Copy source data"

    s3.put_object(Bucket=BUCKET_NAME, Key=src_key, Body=content)
    src_resp = s3.head_object(Bucket=BUCKET_NAME, Key=src_key)
    src_etag = src_resp['ETag']

    print(f"Copying {src_key} -> {dest_key}")
    s3.copy_object(
        Bucket=BUCKET_NAME,
        Key=dest_key,
        CopySource={'Bucket': BUCKET_NAME, 'Key': src_key}
    )

    dest_resp = s3.head_object(Bucket=BUCKET_NAME, Key=dest_key)
    print(f"Source ETag: {src_etag}")
    print(f"Dest ETag:   {dest_resp['ETag']}")

    assert dest_resp['ETag'] == src_etag, "ETag not propagated correctly in CopyObject"
    assert dest_resp['LastModified'] >= src_resp['LastModified'], "LastModified not updated"
    print("CopyObject Metadata: SUCCESS")

def main():
    s3 = get_s3_client()
    print(f"Starting End-to-End Checksum Functional Tests on bucket: {BUCKET_NAME}")

    try:
        s3.create_bucket(Bucket=BUCKET_NAME)

        test_put_object_with_checksum(s3)
        test_multipart_upload_checksum(s3)
        test_copy_object_metadata(s3)

        print("\nALL CHECKSUM FUNCTIONAL TESTS PASSED")
    except Exception as e:
        print(f"\nTEST FAILED: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
    finally:
        # Cleanup
        try:
            print(f"\nCleaning up bucket {BUCKET_NAME}...")
            objs = s3.list_objects_v2(Bucket=BUCKET_NAME)
            if 'Contents' in objs:
                for o in objs['Contents']:
                    s3.delete_object(Bucket=BUCKET_NAME, Key=o['Key'])
            s3.delete_bucket(Bucket=BUCKET_NAME)
        except:
            pass

if __name__ == "__main__":
    main()
