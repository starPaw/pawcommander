(module
  (memory (export "memory") 1)
  (func (export "paw_preview") (param $length i32) (result i64)
    (local $index i32)
    (local $lines i32)
    (local $digit i32)
    (block $end
      (loop $scan
        local.get $index
        local.get $length
        i32.ge_u
        br_if $end
        local.get $lines
        local.get $index
        i32.load8_u
        i32.const 10
        i32.eq
        i32.add
        local.set $lines
        local.get $index
        i32.const 1
        i32.add
        local.set $index
        br $scan
      )
    )
    (if (local.get $length)
      (then
        local.get $lines
        local.get $length
        i32.const 1
        i32.sub
        i32.load8_u
        i32.const 10
        i32.ne
        i32.add
        local.set $lines
      )
    )

    ;; Write the decimal result backward from the end of the memory page.
    i32.const 65536
    local.set $digit
    (loop $decimal
      local.get $digit
      i32.const 1
      i32.sub
      local.tee $digit
      local.get $lines
      i32.const 10
      i32.rem_u
      i32.const 48
      i32.add
      i32.store8
      local.get $lines
      i32.const 10
      i32.div_u
      local.tee $lines
      br_if $decimal
    )

    ;; The high 32 bits are the output address and the low bits are its length.
    local.get $digit
    i64.extend_i32_u
    i64.const 32
    i64.shl
    i32.const 65536
    local.get $digit
    i32.sub
    i64.extend_i32_u
    i64.or
  )
)
