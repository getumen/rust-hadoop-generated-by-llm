"""
Spark S3 Integration Test - Minimal Version

This script tests Apache Spark's ability to read and write data
using the custom DFS S3 server as a backend storage.
"""

from pyspark.sql import SparkSession
from pyspark.sql.types import StructType, StructField, StringType, IntegerType


def create_spark_session():
    """Create a Spark session configured to use custom S3 endpoint."""
    # All S3A configs are passed via spark-submit command line
    spark = SparkSession.builder \
        .appName("S3IntegrationTest") \
        .getOrCreate()
    return spark


def test_write_and_read_csv(spark, bucket_name):
    """Test writing and reading a CSV file to/from S3 with content verification."""
    print("\n=== Test: Write and Read CSV ===")

    # Create test data
    data = [
        ("Alice", 30, "Engineering"),
        ("Bob", 25, "Marketing"),
        ("Charlie", 35, "Sales"),
    ]

    schema = StructType([
        StructField("name", StringType(), True),
        StructField("age", IntegerType(), True),
        StructField("department", StringType(), True),
    ])

    df = spark.createDataFrame(data, schema)
    output_path = f"s3a://{bucket_name}/test-data/employees.csv"

    # Write CSV
    df.write.mode("overwrite").option("header", "true").csv(output_path)
    print("CSV write successful!")

    # Read back
    read_df = spark.read.option("header", "true").csv(output_path)
    # Cast age to int as CSV read returns strings by default unless inferSchema is used
    from pyspark.sql.functions import col
    read_df = read_df.withColumn("age", col("age").cast(IntegerType()))

    # Verify row count
    assert read_df.count() == 3, f"Expected 3 rows, got {read_df.count()}"

    # Verify content
    collected_data = sorted([(row.name, row.age, row.department) for row in read_df.collect()])
    original_data = sorted(data)

    assert collected_data == original_data, f"Data mismatch! Expected {original_data}, got {collected_data}"
    print("CSV content verification PASSED!")


def test_write_and_read_parquet(spark, bucket_name):
    """Test writing and reading Parquet files."""
    print("\n=== Test: Write and Read Parquet ===")

    data = [
        (1, "Product A", 100.0),
        (2, "Product B", 200.0),
        (3, "Product C", 150.0),
    ]
    schema = StructType([
        StructField("id", IntegerType(), True),
        StructField("product", StringType(), True),
        StructField("price", StringType(), True), # Using string for simplicity in minimal test
    ])

    df = spark.createDataFrame(data, schema)
    output_path = f"s3a://{bucket_name}/test-data/products.parquet"

    df.write.mode("overwrite").parquet(output_path)
    print("Parquet write successful!")

    read_df = spark.read.parquet(output_path)
    assert read_df.count() == 3

    collected_data = sorted([(row.id, row.product, row.price) for row in read_df.collect()])
    original_data = sorted(data)
    assert collected_data == original_data
    print("Parquet verification PASSED!")


def test_complex_operations(spark, bucket_name):
    """Test SQL and aggregations on S3 data."""
    print("\n=== Test: SQL and Aggregations ===")

    output_path = f"s3a://{bucket_name}/test-data/employees.csv"
    df = spark.read.option("header", "true").inferSchema(True).csv(output_path)
    df.createOrReplaceTempView("employees")

    # SQL Test
    result_df = spark.sql("SELECT department, AVG(age) as avg_age FROM employees GROUP BY department")
    results = {row.department: row.avg_age for row in result_df.collect()}

    assert results["Engineering"] == 30.0
    assert results["Marketing"] == 25.0
    assert results["Sales"] == 35.0

    print("SQL aggregation verification PASSED!")


def main():
    print("=" * 60)
    print("Spark S3 Integration Test - Enhanced")
    print("=" * 60)

    bucket_name = "spark-test-bucket"

    # Create Spark session
    spark = create_spark_session()
    print(f"Spark version: {spark.version}")

    try:
        test_write_and_read_csv(spark, bucket_name)
        test_write_and_read_parquet(spark, bucket_name)
        test_complex_operations(spark, bucket_name)

        print("\n" + "=" * 60)
        print("ALL TESTS PASSED!")
        print("=" * 60)

    except Exception as e:
        print(f"\n\nTEST FAILED with error: {e}")
        import traceback
        traceback.print_exc()
        raise e
    finally:
        spark.stop()


if __name__ == "__main__":
    main()
