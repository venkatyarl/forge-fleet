package storage

import (
	"bytes"
	"context"
	"fmt"
	"io"

	"github.com/minio/minio-go/v7"
	"github.com/minio/minio-go/v7/pkg/credentials"
	"github.com/venkatyarl/forge-fleet/pkg/config"
)

// MinIOClient stores artifacts in a single MinIO bucket.
type MinIOClient struct {
	client *minio.Client
	bucket string
}

// NewMinIOStore creates an artifact store using the configured MinIO endpoint.
func NewMinIOStore(cfg config.MinIO) (*MinIOClient, error) {
	client, err := minio.New(cfg.Endpoint, &minio.Options{
		Creds:  credentials.NewStaticV4(cfg.AccessKey, cfg.SecretKey, ""),
		Secure: cfg.Secure,
	})
	if err != nil {
		return nil, fmt.Errorf("create MinIO client: %w", err)
	}

	return &MinIOClient{client: client, bucket: cfg.Bucket}, nil
}

// UploadArtifact uploads data under key, replacing an existing object with the
// same key.
func (m *MinIOClient) UploadArtifact(ctx context.Context, key string, data []byte) error {
	_, err := m.client.PutObject(
		ctx,
		m.bucket,
		key,
		bytes.NewReader(data),
		int64(len(data)),
		minio.PutObjectOptions{ContentType: "application/octet-stream"},
	)
	if err != nil {
		return fmt.Errorf("upload artifact %q: %w", key, err)
	}
	return nil
}

// DownloadArtifact downloads the complete object stored under key.
func (m *MinIOClient) DownloadArtifact(ctx context.Context, key string) ([]byte, error) {
	object, err := m.client.GetObject(ctx, m.bucket, key, minio.GetObjectOptions{})
	if err != nil {
		return nil, fmt.Errorf("get artifact %q: %w", key, err)
	}
	defer object.Close()

	data, err := io.ReadAll(object)
	if err != nil {
		return nil, fmt.Errorf("download artifact %q: %w", key, err)
	}
	return data, nil
}

// HealthCheck verifies that MinIO is reachable and the configured bucket is
// accessible.
func (m *MinIOClient) HealthCheck(ctx context.Context) error {
	exists, err := m.client.BucketExists(ctx, m.bucket)
	if err != nil {
		return fmt.Errorf("check MinIO bucket %q: %w", m.bucket, err)
	}
	if !exists {
		return fmt.Errorf("MinIO bucket %q does not exist", m.bucket)
	}
	return nil
}
