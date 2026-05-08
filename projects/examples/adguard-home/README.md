# adguard-home — orca example plugin (Go)

Polls an [AdGuard Home](https://github.com/AdguardTeam/AdGuardHome) instance's
`/control/stats` endpoint and publishes the rolling 24h DNS query stats into
the `network:dns` context every 30 seconds.

## Build

```sh
cd projects/examples/adguard-home
go build -o bin/orca-example-adguard-home .
```

## Run standalone

```sh
orca pki issue adguard-home
ORCA_PLUGIN_ADDR=127.0.0.1:5051 \
ORCA_PKI_DIR=$HOME/.orca/pki \
ORCA_PLUGIN_ID=adguard-home \
ADGUARD_BASE_URL=http://10.0.0.2:3000 \
ADGUARD_USERNAME=admin ADGUARD_PASSWORD=changeme \
./bin/orca-example-adguard-home
```

Published TypedValue:

```json
{
  "type": "adguard-home.DnsQueryStats",
  "schema_version": "0.1.0",
  "sensitivity": "general",
  "payload": {
    "host": "router.local",
    "num_dns_queries": 481203,
    "num_blocked_filtering": 39204,
    "num_replaced_safebrowsing": 12,
    "num_replaced_parental": 0,
    "avg_processing_time": 0.0021
  }
}
```

API reference: https://github.com/AdguardTeam/AdGuardHome/wiki/API
