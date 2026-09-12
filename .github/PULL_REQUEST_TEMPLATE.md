## What

<!-- 可观测的结果与改动范围 -->

## Why

<!-- 用户需求、根因，或已确认的设计决策 -->

## Issue

<!-- 草稿评审期用 Related #N，避免提前关闭；获得合并授权后改为 Fixes #N -->
Related #

## Construction plan

<!-- 一个勾选项一个具体实现步骤。只勾已提交且已验证的 -->
- [ ]

## Delivery record

<!-- 实际改动的文件、commit、执行过的命令与结果。失败原样保留 -->

## How to test

```bash
```

## Checklist

- [ ] Issue 关系正确（草稿期 Related，合并前才改 Fixes）
- [ ] 已加类型与范围标签
- [ ] diff 聚焦，无密钥与生成物
- [ ] `cargo fmt --all --check` 通过
- [ ] `cargo clippy --all-targets -- -D warnings` 无告警
- [ ] `cargo test --all` 通过
- [ ] `./scripts/check-purity.sh` 通过
