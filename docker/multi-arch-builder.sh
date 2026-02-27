#!/usr/bin/env bash

docker run --rm --privileged multiarch/qemu-user-static --reset -p yes
docker buildx rm builder
docker buildx create --name builder --driver docker-container --use
docker buildx inspect --bootstrap
sudo docker buildx build --platform linux/amd64 --tag podcastindexorg/podcasting20-podping.alpha/frontend:3.0.0 --tag podcastindexorg/podcasting20-podping.alpha/frontend:latest --no-cache --output "type=registry" .