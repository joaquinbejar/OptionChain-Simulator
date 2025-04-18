CREATE TABLE IF NOT EXISTS ohlcv
(
    symbol
    String,
    timestamp
    DateTime,
    open
    Float32,
    high
    Float32,
    low
    Float32,
    close
    Float32,
    volume
    UInt32
) ENGINE = MergeTree
    ORDER BY
(
    symbol,
    timestamp
);