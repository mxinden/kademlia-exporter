FROM rustlang/rust:nightly-bullseye as builder
WORKDIR /usr/src/kademlia-exporter

# Cache dependencies between test runs,
# See https://blog.mgattozzi.dev/caching-rust-docker-builds/
# And https://github.com/rust-lang/cargo/issues/2644

RUN mkdir -p ./src/
RUN echo "fn main() {}" > ./src/main.rs
COPY ./Cargo.* ./
RUN cargo +nightly build

COPY . .
RUN cargo +nightly build --release

FROM debian:bullseye-slim
COPY --from=builder /usr/src/kademlia-exporter/target/release/kademlia-exporter /usr/local/bin/kademlia-exporter
ENTRYPOINT [ "kademlia-exporter"]
