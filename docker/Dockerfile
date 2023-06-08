# Stage 1: Build the Rust app
FROM rust:1.70 as build
WORKDIR /usr/src/app
COPY . .
RUN cargo install --path .

# Stage 2: Copy the built binary into a slim Debian-based image
FROM rust:1.70
# RUN apt-get update && apt-get install -y libssl1.1 && apt clean && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/cargo/bin/ldk-sample /usr/local/bin/ldk-sample
CMD ["ldk-sample", "user:password@localhost:18443", "dataldk0/", "63963", "9735", "regtest"]
# CMD ["/target/debug/docker", "user:password@localhost:18443 dataldk0/ 63963 9735 regtest"]