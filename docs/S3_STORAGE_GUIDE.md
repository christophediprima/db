# Fluree S3 Storage Configuration Guide

## Overview

Fluree DB supports Amazon S3 and S3-compatible storage services as a storage backend for ledger data, commits, and indexes. This includes:
- **Amazon S3** (AWS)
- **Google Cloud Storage (GCS)**
- **MinIO** (self-hosted or cloud)
- **DigitalOcean Spaces**
- **Wasabi**
- **Backblaze B2**
- Any other S3-compatible object storage

This guide covers configuration, usage, and production deployment of S3 and S3-compatible storage.

## Configuration

### Basic S3 Configuration

```json
{
  "@context": {
    "@vocab": "https://ns.flur.ee/system#"
  },
  "@graph": [
    {
      "@id": "s3Storage",
      "@type": "Storage",
      "s3Bucket": "your-bucket-name",
      "s3Endpoint": "https://s3.us-east-1.amazonaws.com",
      "s3Prefix": "ledgers",
      "addressIdentifier": "production-s3"
    }
  ]
}
```

### Configuration Options

| Field | Required | Description | Example |
|-------|----------|-------------|---------|
| `s3Bucket` | Yes | S3 bucket name | `"fluree-production-data"` |
| `s3Endpoint` | **Yes** | S3 endpoint URL | `"https://s3.us-
east-1.amazonaws.com"` |
| `s3Prefix` | No | Key prefix for all objects | `"ledgers"` |
| `addressIdentifier` | No | Unique identifier for this storage instance |
`"prod-s3"` |

> **Note**: As of the latest version, `s3Endpoint` is a required parameter. The
API will throw a validation error if not provided.

### Complete System Configuration

```json
{
  "@context": {
    "@vocab": "https://ns.flur.ee/system#"
  },
  "@graph": [
    {
      "@id": "s3Storage",
      "@type": "Storage",
      "s3Bucket": "fluree-production-data",
      "s3Endpoint": "https://s3.us-east-1.amazonaws.com",
      "s3Prefix": "ledgers",
      "addressIdentifier": "prod-s3"
    },
    {
      "@id": "connection",
      "@type": "Connection",
      "parallelism": 4,
      "cacheMaxMb": 1000,
      "commitStorage": {"@id": "s3Storage"},
      "indexStorage": {"@id": "s3Storage"},
      "primaryPublisher": {
        "@type": "Publisher",
        "storage": {"@id": "s3Storage"}
      }
    }
  ]
}
```

## AWS Credentials

### Authentication Methods
Fluree uses the AWS SDK's default credential chain:

1. **Environment Variables**
   ```bash
   export AWS_ACCESS_KEY_ID=your_access_key
   export AWS_SECRET_ACCESS_KEY=your_secret_key
   export AWS_REGION=us-east-1
   ```

2. **AWS Credentials File** (`~/.aws/credentials`)
   ```ini
   [default]
   aws_access_key_id = your_access_key
   aws_secret_access_key = your_secret_key
   region = us-east-1
   ```

3. **IAM Roles** (when running on EC2)
   - Automatically uses instance profile credentials

4. **AWS CLI Configuration**
   ```bash
   aws configure
   ```

### Required S3 Permissions

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:ListBucket"
      ],
      "Resource": [
        "arn:aws:s3:::your-bucket-name",
        "arn:aws:s3:::your-bucket-name/*"
      ]
    }
  ]
}
```

## S3-Compatible Storage Services

Fluree supports S3-compatible storage services in production through the `endpoint-override` parameter. The implementation automatically uses path-style URLs for custom endpoints and virtual-hosted-style URLs for AWS S3.

### Google Cloud Storage (GCS)

**Setup:**
1. Create a bucket in Google Cloud Console
2. Generate HMAC keys (Interoperability tab in Cloud Storage settings)
3. Use the HMAC keys as AWS credentials

```bash
export AWS_ACCESS_KEY_ID="GOOG1E..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_REGION="us-east1"
```

**Configuration:**
```clojure
(require '[fluree.db.storage.s3 :as s3])

(def gcs-store 
  (s3/open "fluree-prod"
           "my-fluree-bucket"
           "ledgers/"
           "https://storage.googleapis.com"
           {:write-timeout-ms 120000  ; Increase for large writes
            :max-retries 5}))
```

**Notes:**
- GCS uses path-style URLs: `https://storage.googleapis.com/bucket/path`
- HMAC keys provide S3-compatible authentication
- GCS requires `auto` as the region in AWS Signature V4 (automatically detected and handled)
- The `AWS_REGION` environment variable can be set to any value for GCS (e.g., `us-east-1`)
- Fluree automatically detects GCS endpoints and uses the correct signature format

### MinIO

**Setup:**
```bash
# Start MinIO with Docker
docker run -p 9000:9000 -p 9001:9001 \
  -e "MINIO_ROOT_USER=minioadmin" \
  -e "MINIO_ROOT_PASSWORD=minioadmin" \
  minio/minio server /data --console-address ":9001"

# Set credentials
export AWS_ACCESS_KEY_ID="minioadmin"
export AWS_SECRET_ACCESS_KEY="minioadmin"
```

**Configuration:**
```clojure
;; Local development
(def minio-local
  (s3/open "dev" "fluree-data" "dev/" "http://localhost:9000"))

;; Production deployment
(def minio-prod
  (s3/open "prod" "fluree-data" "prod/" "https://minio.example.com"))
```

**Benefits:**
- Self-hosted, full control over data
- Compatible with AWS S3 API
- Can run on-premises or in any cloud
- Lower costs for large-scale storage

### DigitalOcean Spaces

**Setup:**
1. Create a Space in DigitalOcean control panel
2. Generate Spaces access keys
3. Note the endpoint for your region

```bash
export AWS_ACCESS_KEY_ID="your-spaces-key"
export AWS_SECRET_ACCESS_KEY="your-spaces-secret"
```

**Configuration:**
```clojure
(def do-store
  (s3/open "prod" 
           "my-fluree-space" 
           "ledgers/"
           "https://nyc3.digitaloceanspaces.com"))
```

**Available regions:**
- `nyc3.digitaloceanspaces.com` (New York)
- `sfo3.digitaloceanspaces.com` (San Francisco)
- `sgp1.digitaloceanspaces.com` (Singapore)
- `fra1.digitaloceanspaces.com` (Frankfurt)
- `ams3.digitaloceanspaces.com` (Amsterdam)

### Other S3-Compatible Services

**Wasabi:**
```clojure
(def wasabi-store
  (s3/open "prod" "my-bucket" "ledgers/" 
           "https://s3.wasabisys.com"))
```

**Backblaze B2:**
```clojure
(def b2-store
  (s3/open "prod" "my-bucket" "ledgers/" 
           "https://s3.us-west-000.backblazeb2.com"))
```

### URL Styles and Signature Calculation

Fluree automatically handles URL formatting and AWS Signature V4 calculation:

- **AWS S3**: Virtual-hosted-style URLs
  ```
  URL:  https://bucket.s3.region.amazonaws.com/path
  Host: bucket.s3.region.amazonaws.com
  Canonical URI: /path
  Signature Region: actual region (e.g., us-east-1)
  ```

- **S3-Compatible Services**: Path-style URLs
  ```
  URL:  https://endpoint/bucket/path
  Host: endpoint (e.g., storage.googleapis.com)
  Canonical URI: /bucket/path
  Signature Region: actual region (or 'auto' for GCS)
  ```

**Special Cases:**
- **Google Cloud Storage**: Automatically uses `auto` as the signature region (detected by `googleapis.com` in endpoint)
- **MinIO, DigitalOcean Spaces, etc.**: Use the actual region from `AWS_REGION` environment variable

This ensures both the request URL and signature calculation match what each service expects. The signature's canonical request must align with the actual HTTP request structure.

### Authentication for S3-Compatible Services

All S3-compatible services use the same AWS credential environment variables:

```bash
export AWS_ACCESS_KEY_ID="your-access-key"
export AWS_SECRET_ACCESS_KEY="your-secret-key"
export AWS_REGION="us-east-1"  # Optional, may be ignored by some services
```

For services that provide HMAC keys or S3-compatible credentials, use those values directly.

### Migration Between Storage Services

To migrate from one storage service to another:

1. **Export data** from the source storage
2. **Set up** the target S3-compatible storage
3. **Update configuration** with new endpoint
4. **Copy data** using tools like `rclone` or `s3cmd`:
   ```bash
   # Example: AWS S3 to MinIO
   rclone copy s3-source:bucket/prefix minio-dest:bucket/prefix
   ```
5. **Update credentials** for the new service
6. **Test** with a non-production ledger first
7. **Switch** production traffic

## API Reference

### Using connect-s3 (Recommended)

```clojure
(require '[fluree.db.api :as fluree])

;; S3 connection for AWS
(def aws-conn
  @(fluree/connect-s3
    {:s3-bucket "my-fluree-bucket"
     :s3-endpoint "https://s3.us-east-1.amazonaws.com"
     :s3-prefix "ledgers/"
     :parallelism 4
     :cache-max-mb 1000}))

;; S3 connection for LocalStack (testing)
(def localstack-conn
  @(fluree/connect-s3
    {:s3-bucket "fluree-test"
     :s3-endpoint "http://localhost:4566"
     :s3-prefix "test/"
     :parallelism 2
     :cache-max-mb 500}))
```

### Direct S3Storage Constructor

The `s3/open` function provides direct access to S3 storage:

```clojure
(require '[fluree.db.storage.s3 :as s3])

;; AWS S3 (default)
(s3/open identifier bucket prefix)
;; or explicitly
(s3/open identifier bucket prefix nil)

;; S3-compatible storage with custom endpoint
(s3/open identifier bucket prefix endpoint-override)

;; With additional options
(s3/open identifier bucket prefix endpoint-override
         {:read-timeout-ms 20000
          :write-timeout-ms 60000
          :list-timeout-ms 20000
          :max-retries 4
          :retry-base-delay-ms 150
          :retry-max-delay-ms 2000})
```

**Parameters:**
- `identifier` - Unique identifier for this storage instance (can be nil)
- `bucket` - S3 bucket name
- `prefix` - Optional prefix for all keys (e.g., "myapp/data/")
- `endpoint-override` - Optional custom endpoint for S3-compatible storage
  - `nil` or omitted: Uses AWS S3 (virtual-hosted-style URLs)
  - Custom endpoint: Uses path-style URLs for S3-compatible services

**Examples:**

```clojure
;; AWS S3
(def aws-store (s3/open "prod" "my-bucket" "ledgers/"))

;; Google Cloud Storage
(def gcs-store (s3/open "prod" "my-bucket" "ledgers/" 
                        "https://storage.googleapis.com"))

;; MinIO (local development)
(def minio-store (s3/open "dev" "fluree-data" "dev/" 
                          "http://localhost:9000"))

;; MinIO (production)
(def minio-prod (s3/open "prod" "fluree-data" "prod/" 
                         "https://minio.example.com"))

;; DigitalOcean Spaces
(def do-store (s3/open "prod" "my-bucket" "ledgers/" 
                       "https://nyc3.digitaloceanspaces.com"))
```

### Storage Protocols
- `(storage/write-bytes store path data)` - Write raw bytes
- `(storage/read-bytes store path)` - Read raw bytes  
- `(storage/-content-write-bytes store dir data)` - Content-addressed write
- `(storage/-read-json store address keywordize?)` - Read JSON document
- `(storage/location store)` - Get storage location URI
- `(storage/identifiers store)` - Get storage identifier set

### Configuration Schema
See `fluree.db.connection.vocab` for complete configuration vocabulary
definitions.

## Production Considerations

### Performance
- Use appropriate instance types with sufficient network bandwidth
- For AWS S3: Consider S3 Transfer Acceleration for global deployments
- For self-hosted (MinIO): Ensure adequate disk I/O and network capacity
- Monitor request costs and optimize access patterns
- For AWS S3: Use S3 Intelligent Tiering for cost optimization
- For S3-compatible services: Configure appropriate storage classes/tiers
- Consider geographic proximity between compute and storage

### Security
- **AWS S3**: Use IAM roles instead of access keys when possible
- **S3-Compatible**: Secure credential management for access keys
- Enable bucket/object encryption at rest
- Implement bucket policies or access control lists
- Enable access logging for audit trails
- **AWS S3**: Use VPC endpoints for private S3 access
- **Self-hosted**: Configure network security groups and firewalls
- Use HTTPS endpoints in production (not HTTP)

### Monitoring
- **AWS S3**: Set up CloudWatch alarms for S3 operations
- **S3-Compatible**: Configure service-specific monitoring (Prometheus for MinIO, etc.)
- Monitor storage costs and usage patterns
- Track Fluree application metrics
- Implement health checks for storage connectivity
- Monitor request latencies and error rates

### Backup and Recovery
- Enable S3 versioning for data protection
- Set up cross-region replication if needed
- Implement regular backup verification
- Document recovery procedures

### Deployment Checklist
- [ ] Storage service selected (AWS S3, GCS, MinIO, etc.)
- [ ] Credentials configured (IAM roles or access keys)
- [ ] Bucket/container created with proper permissions
- [ ] Endpoint URL configured correctly
- [ ] Network connectivity verified (VPC endpoints, firewalls, etc.)
- [ ] Monitoring and alerting enabled
- [ ] Backup and recovery procedures documented
- [ ] Performance tuning applied
- [ ] Cost optimization enabled (lifecycle policies, storage tiers)
- [ ] Security measures implemented (encryption, access control)
- [ ] Testing completed with non-production data

## Migration from Other Storage

### From File Storage to S3
1. Export existing ledger data
2. Configure S3 storage
3. Import data to new S3-backed system
4. Verify data integrity
5. Update application configuration

### Performance Comparison
- File storage: Lower latency, higher IOPS
- S3 storage: Higher durability, infinite scalability
- Choose based on performance vs. durability requirements

## Troubleshooting

### Common Issues

#### 1. Authentication Failures
**Symptoms**: Access denied errors, credential errors, `SignatureDoesNotMatch`
**Solutions**:
- Verify AWS credentials are properly configured
- Check IAM permissions for the bucket
- Ensure AWS region is correctly set
- Test credentials with AWS CLI: `aws s3 ls s3://your-bucket`
- **For GCS**: Verify HMAC keys are correct - test with `gsutil` using the same credentials
- **For GCS**: Ensure you're using HMAC keys, not service account JSON keys
- Check that the secret key doesn't have extra whitespace or newlines

#### 2. Bucket Access Issues
**Symptoms**: NoSuchBucket, AccessDenied errors
**Solutions**:
- Verify bucket name is correct and exists
- Check bucket permissions and policies
- Ensure bucket is in the correct region
- Verify network connectivity to S3

#### 3. Network/Connectivity Issues
**Symptoms**: Timeout errors, connection refused
**Solutions**:
- Check firewall rules and security groups
- Verify S3 endpoint configuration
- Test network connectivity: `ping s3.amazonaws.com`
- For custom endpoints, verify service is running

#### 4. Configuration Issues
**Symptoms**: ClassNotFound, protocol errors
**Solutions**:
- Verify S3 dependencies are included in classpath
- Check configuration JSON-LD syntax
- Ensure all required configuration fields are present
- Validate configuration parsing with test utilities

#### 5. Index Loading Issues
**Symptoms**: "Error resolving index node" when loading ledgers from cold start
**Solutions**:
- This was a known bug fixed in recent versions
- Ensure you're using the latest version with the index loading fix
- The issue was caused by improper address resolution in S3Store's `-read-json`
    method
- If still experiencing issues, verify fluree addresses are properly formatted

#### 6. connect-s3 API Validation Errors
**Symptoms**: "S3 bucket name is required" or "S3 endpoint is required" errors
**Solutions**:
- Ensure both `s3-bucket` and `s3-endpoint` parameters are provided
- `s3-endpoint` is now a required parameter (changed from optional)
- Example: `{:s3-bucket "my-bucket" :s3-endpoint "http://localhost:4566"}`
- For AWS: use `"https://s3.us-east-1.amazonaws.com"` format
- For LocalStack: use `"http://localhost:4566"` format

---

## Implementation Details

### S3 Storage Implementation

#### Features
- **Content-Addressed Storage**: Automatic SHA-256 hashing and addressing
- **JSON Archive Support**: Direct JSON read/write with compression
- **Byte Store**: Raw byte storage and retrieval
- **AWS SDK Integration**: Uses Cognitect AWS SDK for reliable S3 operations
- **Async Operations**: All S3 operations are asynchronous using core.async

#### Storage Protocols
The S3 implementation satisfies all required storage protocols:
- `storage/Addressable` - Provides fluree address generation
- `storage/Identifiable` - Storage identifier management
- `storage/JsonArchive` - JSON document storage
- `storage/ContentAddressedStore` - Hash-based content storage
- `storage/ByteStore` - Raw byte operations

#### S3 File Structure
- Ledger metadata: `<prefix>/<ledger-name>.json`
- Commit files: `<prefix>/<ledger-name>/commit/<hash>.json`
- Index files: `<prefix>/<ledger-
    name>/index/{root,post,spot,tspo,opst}/<hash>.json`

## Testing and Development

### Test Organization

The S3 storage tests are organized into three categories:

1. **Unit Tests** (`s3_unit_test.clj`)
   - Pure unit tests without external dependencies
   - Protocol compliance verification
   - Basic S3Store creation and configuration
   - API parameter validation
   - S3-compatible endpoint URL generation tests
   - No LocalStack or real S3 required

2. **Integration Tests** (`s3_test.clj`)
   - Basic S3 integration with LocalStack
   - Real S3 read/write operations
   - End-to-end ledger workflows
   - Requires LocalStack running

3. **Indexing Tests** (`s3_indexing_test.clj`)
   - Comprehensive indexing functionality
   - Index creation and storage validation
   - Cold start index loading
   - Query functionality with indexes
   - Requires LocalStack running

### Test Environment Setup

#### Option 1: LocalStack (Recommended for Development)

1. **Start LocalStack**
   ```bash
   docker run -p 4566:4566 localstack/localstack
   ```

2. **Set Environment Variables**
   ```bash
   export S3_TEST_ENDPOINT=http://localhost:4566
   export S3_TEST_BUCKET=fluree-test-bucket
   export AWS_ACCESS_KEY_ID=test
   export AWS_SECRET_ACCESS_KEY=test
   export AWS_REGION=us-east-1
   ```

3. **Run Tests**
   ```bash
   # Unit tests (no LocalStack required)
   clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-unit-test) (clojure.test/run-tests 'fluree.db.storage.s3-unit-test)"
   
   # Integration tests (requires LocalStack)
   clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-test) (clojure.test/run-tests 'fluree.db.storage.s3-test)"
   
   # Indexing tests (requires LocalStack)
   clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-indexing-test) (clojure.test/run-tests 'fluree.db.storage.s3-indexing-test)"
   ```

#### Option 2: Real AWS S3

1. **Set AWS Credentials** (see Authentication Methods above)

2. **Create Test Bucket**
   ```bash
   aws s3 mb s3://fluree-test-bucket
   ```

3. **Set Environment Variables**
   ```bash
   export S3_TEST_BUCKET=fluree-test-bucket
   export S3_TEST_PREFIX=test-data
   # Don't set S3_TEST_ENDPOINT for real AWS
   ```

4. **Run Integration Tests**
   ```bash
   # Integration tests
   clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-test) (clojure.test/run-tests 'fluree.db.storage.s3-test)"
   
   # Indexing tests
   clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-indexing-test) (clojure.test/run-tests 'fluree.db.storage.s3-indexing-test)"
   ```

### Running Tests

```bash
# All S3 tests
make test  # Runs all tests including S3

# Unit tests only (no external dependencies)
clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-unit-test) (clojure.test/run-tests 'fluree.db.storage.s3-unit-test)"

# Integration tests only (requires LocalStack)
clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-test) (clojure.test/run-tests 'fluree.db.storage.s3-test)"

# Indexing tests only (requires LocalStack)
clojure -M:dev:cljtest -e "(require 'fluree.db.storage.s3-indexing-test) (clojure.test/run-tests 'fluree.db.storage.s3-indexing-test)"
```

### Debugging Tools

#### Enable S3 Debug Logging
```xml
<!-- Add to logback.xml -->
<logger name="fluree.db.storage.s3" level="DEBUG"/>
<logger name="cognitect.aws" level="DEBUG"/>
<logger name="fluree.db.api" level="INFO"/>
<logger name="f.db.flake.index.novelty" level="INFO"/>
<logger name="f.db.nameservice.storage" level="INFO"/>
```

> **Note**: S3 tests now use proper logging via `fluree.db.util.log` instead of
`println` for better control over log levels.

#### Manual S3 Operations
```clojure
(require '[fluree.db.storage.s3 :as s3])
(require '[fluree.db.storage :as storage])
(require '[clojure.core.async :refer [<!]])

;; Create store with AWS S3
(def aws-store (s3/open "test" "your-bucket" "test-prefix"))

;; Create store with custom endpoint (MinIO, GCS, etc.)
(def custom-store (s3/open "test" "your-bucket" "test-prefix" "http://localhost:9000"))

;; Test write
(def result (<! (storage/write-bytes custom-store "test.txt" "Hello, S3!")))

;; Test read
(def content (<! (storage/read-bytes custom-store "test.txt")))

;; Verify URL generation
(s3/build-s3-url "my-bucket" "us-east-1" "path/file.json" nil)
;; => "https://my-bucket.s3.us-east-1.amazonaws.com/path/file.json"

(s3/build-s3-url "my-bucket" "us-east-1" "path/file.json" "http://localhost:9000")
;; => "http://localhost:9000/my-bucket/path/file.json"
```

#### Test Utilities

The `fluree.db.test-utils` namespace provides helpful utilities for S3 testing:

```clojure
(require '[fluree.db.test-utils :as test-utils])
(require '[fluree.db.util.log :as log])

;; Check LocalStack availability
(test-utils/s3-available?) ; Returns true if LocalStack S3 is running at localhost:4566

;; Example usage in tests
(deftest s3-integration-test
  (testing "S3 operations"
    (if-not (test-utils/s3-available?)
      (log/info "⏭️  Skipping S3 test - LocalStack not available")
      (do-s3-integration-test))))
```
