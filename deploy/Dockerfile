# syntax=dockerfile/v1
# pbh-rs 运行镜像：多阶段构建，最终基于 scratch（~15MB）
#
# WebUI 静态资源：构建时从宿主 webui-dist/ 复制（获取方式见 README：
# 上游 webui 源码构建，或从官方发行包 PeerBanHelper.jar 中提取 static/）。

FROM rust:1.90-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin pbh

FROM scratch
COPY --from=builder /build/target/release/pbh /pbh
COPY webui-dist/ /webui/
ENV PBH_DATA_DIR=/data
ENV PBH_STATIC_DIR=/webui
VOLUME /data
EXPOSE 9898
ENTRYPOINT ["/pbh"]
CMD ["run"]
