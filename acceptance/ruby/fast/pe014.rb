# pmap -> Ractors, one per core; interleaved strides for load balance.
require "etc"
N = 1_000_000

def collatz_len(n)
  steps = 1
  while n != 1
    n = n.even? ? n >> 1 : 3 * n + 1
    steps += 1
  end
  steps
end

def work(start, stride)
  best_len = 0
  best_n = 0
  n0 = start
  while n0 < N
    steps = collatz_len(n0)
    if steps > best_len || (steps == best_len && n0 < best_n)
      best_len = steps
      best_n = n0
    end
    n0 += stride
  end
  [best_len, best_n]
end

k = (ENV["RUBY_THREADS"] || Etc.nprocessors).to_i
res =
  if k == 1
    [work(1, 1)]
  else
    Warning[:experimental] = false
    (1..k).map { |s| Ractor.new(s, k) { |s, k| work(s, k) } }.map(&:value)
  end
# smallest n among the longest chains, like max_by on the index list
puts res.max_by { |len, n| [len, -n] }.last
