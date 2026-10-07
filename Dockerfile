FROM alpine:3.22

ARG TARGETARCH

# ca-certificates：rustls 访问上游 HTTPS 需要系统证书
RUN apk add --no-cache ca-certificates \
    && mkdir -p /data

COPY binaries/${TARGETARCH}/gateway /usr/local/bin/gateway

ENV CONFIG_FILE=/data/config.json

WORKDIR /data

VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/gateway"]
