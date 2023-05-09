FROM rust:1.69 as builder
WORKDIR /build
COPY . .
RUN cargo build --release


FROM debian:bullseye-slim
COPY --from=builder /build/target/release/interiris /bin/interiris
ENV RUST_LOG=info
CMD ["/bin/interiris"]
