## Kademlia Exporter

Exporter exposing [Prometheus](https://prometheus.io/) metrics for
[libp2p](https://github.com/libp2p/) Kademlia distributed hash tables.


![Sample Graph](./.assets/random-node-lookup.png)


*Information below is likely outdated. Code is the source of truth.*


### Quickstart

```bash
cargo +nightly run -- --dht-name <dht-name> --dht-bootnode <dht-bootnode>

curl localhost:8080/metrics
```


### Ip localization

Optionally the exporter can estimate a peers location through the [Max Mind Geo DB](https://dev.maxmind.com/geoip/geoip2/geolite2/#Autonomous_System_Numbers).

``` bash
cargo +nightly run -- --dht-name <dht-name> --dht-bootnode <dht-bootnode> --mad-mind-db <path-to-db
```


### Metrics

- Unique nodes discovered within the time bound through the Dht.
  `kademlia_exporter_nodes_seen_within{country,dht,last_seen_within}`

- Unique nodes discovered through the Dht and up since timebound.
  `kademlia_exporter_nodes_up_since{country,dht,up_since}`

- Libp2p network behaviour events.
  `kademlia_exporter_network_behaviour_event{behaviour,dht,event}`

- Duration of random node lookup.
  `random_node_lookup_duration{dht,result}`

- Duration of a ping round trip.
  `kademlia_exporter_ping_duration_cket_count{country,dht}`
