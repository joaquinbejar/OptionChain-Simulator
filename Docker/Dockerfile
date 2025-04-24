FROM rust:1.84.1-bookworm as builder

RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    curl \
    libfontconfig1-dev \
    libfreetype6-dev \
    libexpat1-dev \
    libpng-dev \
    libjpeg-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY . .

RUN make release

FROM debian:bookworm-slim

WORKDIR /app

COPY --from=builder /usr/lib /usr/lib
COPY --from=builder /lib /lib
COPY --from=builder /app/target/release/optionchain_simulator /app/

EXPOSE 7070

CMD ["/app/optionchain_simulator"]