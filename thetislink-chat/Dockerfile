FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml ./Cargo.toml
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/thetislink-chat /usr/local/bin/thetislink-chat
EXPOSE 18082
ENTRYPOINT ["thetislink-chat"]
CMD ["--listen", "0.0.0.0:18082"]
