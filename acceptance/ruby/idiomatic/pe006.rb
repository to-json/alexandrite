n = 100
puts (1..n).sum ** 2 - (1..n).sum { it * it }
