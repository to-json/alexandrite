def collatz_len(n)
  steps = 1
  while n != 1 do n = n.even? ? n / 2 : 3 * n + 1; steps += 1 end
  steps
end
best = (1...1_000_000).to_a.map { collatz_len(it) }.each_with_index.max_by(&:first)
puts best.last + 1
