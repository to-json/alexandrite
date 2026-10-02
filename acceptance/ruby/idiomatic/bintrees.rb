# Binary trees (the Benchmarks Game program, max depth 14).
Node = Struct.new(:left, :right)

def make(d) = d.zero? ? Node.new(nil, nil) : Node.new(make(d - 1), make(d - 1))

def check(n) = n.left ? 1 + check(n.left) + check(n.right) : 1

min_depth = 4
max_depth = 14
puts "stretch tree of depth #{max_depth + 1} check: #{check(make(max_depth + 1))}"
long = make(max_depth)
min_depth.step(max_depth, 2) do |d|
  iters = 1 << (max_depth - d + min_depth)
  total = 0
  iters.times { total += check(make(d)) }
  puts "#{iters} trees of depth #{d} check: #{total}"
end
puts "long lived tree of depth #{max_depth} check: #{check(long)}"
