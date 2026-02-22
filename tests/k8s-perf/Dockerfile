FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libsasl2-2 zlib1g && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 perf
COPY chronik-perf /usr/local/bin/chronik-perf
RUN chmod +x /usr/local/bin/chronik-perf
USER perf
EXPOSE 8080 8081
ENTRYPOINT ["chronik-perf"]
