FROM debian:bookworm-slim AS base

RUN apt update -qy
RUN apt install -qy librocksdb-dev

FROM base as build

RUN apt install -qy git cargo clang cmake

WORKDIR /build
COPY . .

RUN cargo build --release --bin electrs

FROM base as deploy

COPY --from=build /build/target/release/electrs /bin/electrs

EXPOSE 50001

ENTRYPOINT ["/bin/electrs"]