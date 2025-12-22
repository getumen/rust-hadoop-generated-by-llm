"""
Spark S3 Integration Test

This script tests Apache Spark's ability to read and write data
using the custom DFS S3 server as a backend storage.
"""

from pyspark.sql import SparkSession
from pyspark.sql.types import StructType, StructField, StringType, IntegerType
import time


def create_spark_session():
    """Create a Spark session configured to use custom S3 endpoint."""
    spark = SparkSession.builder \
        .appName("S3IntegrationTest") \
        .master("spark://spark-master:7077") \
        .config("spark.hadoop.fs.s3a.endpoint", "http://s3-server:9000") \
        .config("spark.hadoop.fs.s3a.access.key", "dummy") \
        .config("spark.hadoop.fs.s3a.secret.key", "dummy") \
        .config("spark.hadoop.fs.s3a.path.style.access", "true") \
        .config("spark.hadoop.fs.s3a.impl", "org.apache.hadoop.fs.s3a.S3AFileSystem") \
        .config("spark.hadoop.fs.s3a.connection.ssl.enabled", "false") \
        .config("spark.hadoop.fs.s3a.aws.credentials.provider",
                "org.apache.hadoop.fs.s3a.SimpleAWSCredentialsProvider") \
        .config("spark.jars.packages",
                "org.apache.hadoop:hadoop-aws:3.3.4,com.amazonaws:aws-java-sdk-bundle:1.12.262") \
        .getOrCreate()

    return spark


def test_write_csv(spark, bucket_name):
    """Test writing a CSV file to S3."""
    print("\n=== Test: Write CSV to S3 ===")

    # Create test data
    data = [
        ("Alice", 30, "Engineering"),
        ("Bob", 25, "Marketing"),
        ("Charlie", 35, "Sales"),
        ("Diana", 28, "Engineering"),
        ("Eve", 32, "HR"),
    ]

    schema = StructType([
        StructField("name", StringType(), True),
        StructField("age", IntegerType(), True),
        StructField("department", StringType(), True),
    ])

    df = spark.createDataFrame(data, schema)

    output_path = f"s3a://{bucket_name}/test-data/employees.csv"
    print(f"Writing data to: {output_path}")

    df.write.mode("overwrite").csv(output_path, header=True)
    print("CSV write successful!")

    return output_path


def test_read_csv(spark, path):
    """Test reading a CSV file from S3."""
    print("\n=== Test: Read CSV from S3 ===")
    print(f"Reading data from: {path}")

    df = spark.read.csv(path, header=True, inferSchema=True)

    print("Data read successfully!")
    print(f"Row count: {df.count()}")
    df.show()

    return df


def test_write_parquet(spark, bucket_name):
    """Test writing a Parquet file to S3."""
    print("\n=== Test: Write Parquet to S3 ===")

    # Create test data with more columns
    data = [
        (1, "Product A", 100.50, 10),
        (2, "Product B", 200.75, 20),
        (3, "Product C", 300.25, 15),
        (4, "Product D", 150.00, 25),
        (5, "Product E", 250.50, 30),
    ]

    schema = StructType([
        StructField("id", IntegerType(), True),
        StructField("product_name", StringType(), True),
        StructField("price", IntegerType(), True),  # Using int for simplicity
        StructField("quantity", IntegerType(), True),
    ])

    df = spark.createDataFrame(data, schema)

    output_path = f"s3a://{bucket_name}/test-data/products.parquet"
    print(f"Writing Parquet to: {output_path}")

    df.write.mode("overwrite").parquet(output_path)
    print("Parquet write successful!")

    return output_path


def test_read_parquet(spark, path):
    """Test reading a Parquet file from S3."""
    print("\n=== Test: Read Parquet from S3 ===")
    print(f"Reading data from: {path}")

    df = spark.read.parquet(path)

    print("Parquet read successfully!")
    print(f"Row count: {df.count()}")
    df.show()

    # Show schema
    print("\nSchema:")
    df.printSchema()

    return df


def test_sql_operations(spark, df):
    """Test SQL operations on the data."""
    print("\n=== Test: SQL Operations ===")

    # Register as temp view
    df.createOrReplaceTempView("products")

    # Run SQL query
    result = spark.sql("""
        SELECT
            product_name,
            price,
            quantity,
            price * quantity as total_value
        FROM products
        WHERE quantity > 15
        ORDER BY total_value DESC
    """)

    print("SQL query result:")
    result.show()

    return result


def test_aggregation(spark, df):
    """Test aggregation operations."""
    print("\n=== Test: Aggregation ===")

    from pyspark.sql import functions as F

    result = df.agg(
        F.count("*").alias("total_products"),
        F.sum("quantity").alias("total_quantity"),
        F.avg("price").alias("avg_price"),
        F.max("price").alias("max_price"),
        F.min("price").alias("min_price")
    )

    print("Aggregation result:")
    result.show()

    return result


def main():
    print("=" * 60)
    print("Spark S3 Integration Test")
    print("=" * 60)

    bucket_name = "spark-test-bucket"

    # Create Spark session
    print("\nCreating Spark session...")
    spark = create_spark_session()
    print(f"Spark version: {spark.version}")

    try:
        # Test 1: Write CSV
        csv_path = test_write_csv(spark, bucket_name)

        # Test 2: Read CSV
        csv_df = test_read_csv(spark, csv_path)

        # Test 3: Write Parquet
        parquet_path = test_write_parquet(spark, bucket_name)

        # Test 4: Read Parquet
        parquet_df = test_read_parquet(spark, parquet_path)

        # Test 5: SQL Operations
        test_sql_operations(spark, parquet_df)

        # Test 6: Aggregation
        test_aggregation(spark, parquet_df)

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
