# ---- build ----
FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config protobuf-compiler libinput-dev libudev-dev libgbm-dev \
      libdrm-dev libxkbcommon-dev libfontconfig-dev libasound2-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p lcars-app
COPY lcars/bind-udev.c /tmp/bind-udev.c
RUN gcc -O2 -o /tmp/bind-udev /tmp/bind-udev.c

# ---- runtime ----
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      libinput10 libudev1 libgbm1 libdrm2 libegl1 libgles2 libegl-mesa0 \
      libgl1-mesa-dri libxkbcommon0 libfontconfig1 libasound2t64 alsa-utils \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/lcars-app /usr/local/bin/lcars-app
COPY --from=build /tmp/bind-udev /usr/local/bin/bind-udev
COPY assets/ /opt/lcars/assets/
COPY lcars/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 755 /usr/local/bin/entrypoint.sh /usr/local/bin/bind-udev
CMD ["/usr/local/bin/entrypoint.sh"]
