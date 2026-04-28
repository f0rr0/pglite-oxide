# syntax=docker/dockerfile:1.7
FROM ubuntu:24.04@sha256:c4a8d5503dfb2a3eb8ab5f807da5bc69a85730fb49b5cfca2330194ebcc41c7b

ENV DEBIAN_FRONTEND=noninteractive

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
      bash \
      bison \
      build-essential \
      ca-certificates \
      curl \
      file \
      flex \
      gawk \
      git \
      jq \
      lz4 \
      make \
      patch \
      perl \
      pkg-config \
      python3 \
      rsync \
      tar \
      wget \
      xz-utils \
      zstd

ENV HOME=/opt/wasixcc-home
ENV WASIX_HOME=/opt/wasixcc-home/.wasixcc
ENV PATH=/opt/wasixcc-home/.wasixcc/bin:$PATH

RUN mkdir -p /opt/wasixcc-home \
    && curl -fsSL https://wasix.cc -o /tmp/wasixcc-install.sh \
    && HOME=/opt/wasixcc-home sh /tmp/wasixcc-install.sh \
    && wasixcc --version

WORKDIR /work
