#!/bin/bash

set -e

echo "⏳ Waiting for ClickHouse to be ready..."
sleep 3

# Crear tabla
echo "🧱 Creating schema..."
cat ../infrastructure/clickhouse/init.sql | docker exec -i clickhouse clickhouse-client --multiline --multiquery

# Importar CSVs
for file in ../infrastructure/clickhouse/data/*.csv; do
  symbol=$(basename "$file" .csv)
  echo "📥 Inserting data for $symbol"
  awk -F ";" -v sym="$symbol" '{
    split($1, d, "/");
    datetime = sprintf("%04d-%02d-%02d %s", d[3], d[2], d[1], $2);
    printf "%s,%s,%.2f,%.2f,%.2f,%.2f,%d\n", sym, datetime, $3, $4, $5, $6, $7
  }' "$file" | \
  docker exec -i clickhouse clickhouse-client --query="INSERT INTO ohlcv FORMAT CSV"
done

echo "✅ ClickHouse is ready with your data!"