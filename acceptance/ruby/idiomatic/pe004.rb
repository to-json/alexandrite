def palindrome?(n) = (s = n.to_s; s == s.reverse)
puts (100..999).flat_map { |a| (a..999).map { |b| a * b } }.select { palindrome?(it) }.max
