# Build container

FROM rust as build

COPY ./ ./

RUN cargo build --release

RUN mkdir -p /build-out

RUN cp target/release/kademlia-exporter /build-out/


# Final container

FROM ubuntu

COPY --from=build /build-out/kademlia-exporter /

CMD /kademlia-exporter
