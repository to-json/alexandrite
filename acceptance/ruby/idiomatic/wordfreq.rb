LETTERS = "etaoinshrdlucmfwypvbgkjqxz"

def word(n)
  s = +""
  x = n
  loop do
    s << LETTERS[x % 26]
    x /= 26
    break if x.zero?
  end
  s
end

counts = Hash.new(0)
seed = 42
1_000_000.times do
  seed = (seed * 1103515245 + 12345) % 2147483648
  counts[word(seed % 20000)] += 1
end
puts counts.size
counts.sort_by { |w, n| [-n, w] }.first(5).each { |w, n| puts "#{w} #{n}" }
