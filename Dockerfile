# Build container

FROM rustlang/rust:nightly-bullseye as build

COPY ./ ./

RUN cargo +nightly build --release

RUN mkdir -p /build-out

RUN cp target/release/kademlia-exporter /build-out/


# Final container

FROM debian:bullseye-slim

COPY --from=build /build-out/kademlia-exporter /

CMD /kademlia-exporter
