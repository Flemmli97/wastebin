# --- build image

FROM rust:1.80 AS builder

RUN rustup target add x86_64-unknown-linux-musl
RUN apt update && apt install -y musl-tools musl-dev
RUN update-ca-certificates

WORKDIR /app
COPY . .
RUN cargo build --target x86_64-unknown-linux-musl --release

# --- final image

FROM scratch

WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/wastebin ./
CMD ["/app/wastebin"]
