# syntax=dockerfile/v1
# pbh-rs 运行镜像：多阶段构建，最终基于 scratch（~15MB）
# builder 阶段产出 musl 全静态二进制——scratch 无 libc，必须静态链接才能运行。
#
# WebUI 静态资源：构建时从宿主 webui-dist/ 复制（获取方式见 README：
# 上游 webui 源码构建，或从官方发行包 PeerBanHelper.jar 中提取 static/，
# 或直接解压 Release 的 pbh-webui-v9.5.1.zip 为 webui-dist/）。

FROM rust:1.90-bookworm AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rustup target add x86_64-unknown-linux-musl
WORKDIR /build
COPY . .
ENV CC_x86_64_unknown_linux_musl=musl-gcc
RUN cargo build --release --locked --target x86_64-unknown-linux-musl --bin pbh

FROM scratch
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/pbh /pbh
COPY webui-dist/ /webui/
ENV PBH_DATA_DIR=/data
ENV PBH_STATIC_DIR=/webui
VOLUME /data
EXPOSE 9898
ENTRYPOINT ["/pbh"]
CMD ["run"]
