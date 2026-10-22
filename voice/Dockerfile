# ---- build ----
FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config protobuf-compiler libasound2-dev clang cmake g++ \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p voice-daemon

# ---- runtime ----
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      libasound2t64 libstdc++6 libgomp1 alsa-utils ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/voice-daemon /usr/local/bin/voice-daemon
COPY voice/voice.toml /opt/voice/voice.toml
COPY voice/models/ /opt/voice/models/
COPY voice/entrypoint.sh /usr/local/bin/voice-entrypoint.sh
RUN chmod 755 /usr/local/bin/voice-entrypoint.sh
CMD ["/usr/local/bin/voice-entrypoint.sh"]
