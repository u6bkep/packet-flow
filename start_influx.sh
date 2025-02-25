docker run \
    -p 8086:8086 \
    -v "$(pwd)/influx_data:/var/lib/influxdb2" \
    -v "$(pwd)/influx_config:/etc/influxdb2" \
    -e DOCKER_INFLUXDB_INIT_MODE=setup \
    -e DOCKER_INFLUXDB_INIT_USERNAME=ben \
    -e DOCKER_INFLUXDB_INIT_PASSWORD=password \
    -e DOCKER_INFLUXDB_INIT_ORG=home \
    -e DOCKER_INFLUXDB_INIT_BUCKET=packet_monitor \
    influxdb:2