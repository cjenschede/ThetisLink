FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml ./Cargo.toml
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/thetislink-relay /usr/local/bin/thetislink-relay
EXPOSE 18080
ENTRYPOINT ["thetislink-relay"]
CMD ["--listen", "0.0.0.0:18080"]
